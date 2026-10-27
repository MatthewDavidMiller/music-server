//! The application: routing, the status page and its login, and the hand-off
//! of `/rest/...` to the Subsonic API.
//!
//! The status page is read-only and for admins only (the users file's
//! `admin` flag). Its POSTs (log in, log out) require a CSRF token and, when
//! the browser sends them, `Origin` matching `Host` and a `Sec-Fetch-Site`
//! that is not cross-site.

use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ms_common::{rand, timefmt};
use ms_http::{Handler, Method, Request, Response};
use ms_store::lock;
use ms_subsonic::Api;
use ms_subsonic::throttle::Throttle;

use crate::config::Config;
use crate::html::{self, Page, esc};
use crate::session::{LOGIN_COOKIE, SESSION_COOKIE, Sessions};

const STYLE: &str = include_str!("../static/style.css");
const FAVICON: &str = include_str!("../static/favicon.svg");
const LOGIN_FAILURE_LIMIT: usize = 5;
const LOGIN_FAILURE_WINDOW: Duration = Duration::from_mins(15);

#[derive(Debug)]
pub struct App {
    pub config: Config,
    pub api: Api,
    pub sessions: Sessions,
    login_throttle: Throttle,
    started: Instant,
}

fn card(id: &str, heading: &str) -> String {
    format!(
        "<section class=\"card\" aria-labelledby=\"{id}-heading\">\n<h2 id=\"{id}-heading\">{}</h2>\n",
        esc(heading)
    )
}

fn mib(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10) / (1024 * 1024);
    format!("{}.{} MiB", tenths / 10, tenths % 10)
}

fn when(ms: u64) -> String {
    if ms == 0 {
        return "never".to_owned();
    }
    let s = i64::try_from(ms / 1000).unwrap_or(0);
    timefmt::rfc3339_millis(s, 0).replace('T', " ").replace(".000Z", " UTC")
}

fn duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}.{}s", s, ms % 1000 / 100)
    }
}

impl App {
    pub fn new(config: Config, api: Api, sessions: Sessions) -> Self {
        Self {
            config,
            api,
            sessions,
            login_throttle: Throttle::new(LOGIN_FAILURE_LIMIT, LOGIN_FAILURE_WINDOW),
            started: Instant::now(),
        }
    }

    /// The client address used for throttling: the right-most
    /// `X-Forwarded-For` entry when the connection comes from a trusted
    /// proxy (Traefik appends the address it saw), otherwise the peer.
    pub fn client_key(&self, req: &Request) -> String {
        if self.config.trusted_proxies.contains(&req.peer)
            && let Some(ip) = req
                .header("x-forwarded-for")
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            return ip.to_string();
        }
        req.peer.to_string()
    }

    fn cookie(&self, name: &str, value: &str, max_age: u64) -> String {
        let secure = if self.config.secure_cookies { "; Secure" } else { "" };
        format!("{name}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}")
    }

    /// The signed-in admin's session id and name.
    fn session(&self, req: &Request) -> Option<(String, String)> {
        let id = req.cookie(SESSION_COOKIE)?;
        let user = self.sessions.user(id)?;
        self.api.credentials.is_admin(&user).then(|| (id.to_owned(), user))
    }

    fn same_origin(req: &Request) -> bool {
        if req.header("sec-fetch-site").is_some_and(|v| v == "cross-site" || v == "same-site") {
            return false;
        }
        match (req.header("origin"), req.header("host")) {
            (Some(origin), Some(host)) => {
                let authority = origin
                    .strip_prefix("https://")
                    .or_else(|| origin.strip_prefix("http://"))
                    .unwrap_or("");
                authority.eq_ignore_ascii_case(host)
            }
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    fn route(&self, req: &Request) -> Response {
        if req.path.starts_with("/rest/") {
            return self.api.handle(req, &self.client_key(req));
        }
        let post = req.method == Method::Post;
        match (post, req.path.as_str()) {
            (false, "/static/style.css") => Response::new(200, "text/css; charset=utf-8", STYLE)
                .with_header("Cache-Control", "max-age=3600".to_owned()),
            (false, "/static/favicon.svg") => Response::new(200, "image/svg+xml", FAVICON)
                .with_header("Cache-Control", "max-age=3600".to_owned()),
            (false, "/healthz") => Response::text(200, "ok"),
            (false, "/login") => self.login_form(None),
            (true, "/login") => self.login_submit(req),
            (true, "/logout") => self.logout(req),
            (false, "/") => match self.session(req) {
                Some((id, user)) => self.status_page(&id, &user),
                None => Response::redirect("/login"),
            },
            _ => {
                let page = Page {
                    title: "Not found".into(),
                    body: "<h1>Not found</h1>\n<p class=\"page-intro\">There is no page at this address.</p>".into(),
                    ..Page::default()
                };
                Response::html(404, html::render(&page))
            }
        }
    }

    fn login_form(&self, error: Option<&str>) -> Response {
        let Ok(nonce) = rand::random_hex(16) else {
            return Response::text(500, "randomness unavailable");
        };
        let mut body = String::from(
            "<h1>Log in</h1>\n<section class=\"card login-card\" aria-labelledby=\"login-heading\">\n\
             <h2 id=\"login-heading\">Administrator access</h2>\n\
             <p class=\"field-help\">Music is played through a Subsonic app; this page only shows the server's status.</p>\n",
        );
        if let Some(e) = error {
            let _ =
                writeln!(body, "<p class=\"notice notice-danger\" role=\"alert\">{}</p>", esc(e));
        }
        body.push_str("<form method=\"POST\" action=\"/login\">\n");
        body.push_str(&html::csrf_field(&self.sessions.login_token(&nonce)));
        body.push_str(
            "<div class=\"field\">\n<label for=\"username\">Username</label>\n\
             <input type=\"text\" id=\"username\" name=\"username\" autocomplete=\"username\" required autofocus>\n</div>\n\
             <div class=\"field\">\n<label for=\"password\">Password</label>\n\
             <input type=\"password\" id=\"password\" name=\"password\" autocomplete=\"current-password\" required>\n\
             </div>\n<button type=\"submit\" class=\"button\">Log in</button>\n</form>\n</section>",
        );
        let page =
            Page { title: "Log in".into(), page_class: " page-center", body, ..Page::default() };
        Response::html(if error.is_some() { 401 } else { 200 }, html::render(&page))
            .with_header("Set-Cookie", self.cookie(LOGIN_COOKIE, &nonce, 3600))
    }

    fn login_submit(&self, req: &Request) -> Response {
        let form = req.form();
        let nonce = req.cookie(LOGIN_COOKIE).unwrap_or("");
        if !Self::same_origin(req)
            || !self.sessions.verify_login_token(nonce, form.get("csrf_token").unwrap_or(""))
        {
            return self.login_form(Some("The login form expired. Please try again."));
        }
        let client = self.client_key(req);
        if let Some(wait) = self.login_throttle.locked_out(&client) {
            let mut r = self.login_form(Some(&format!(
                "Too many failed attempts. Try again in {} minute(s).",
                wait.div_ceil(60)
            )));
            r.status = 429;
            return r;
        }
        let user = self
            .api
            .credentials
            .check_password(form.text("username"), form.get("password").unwrap_or(""));
        let Some(user) = user.filter(|u| u.admin) else {
            self.login_throttle.record_failure(&client);
            eprintln!("music-server: failed status page login from {client}");
            return self
                .login_form(Some("Incorrect username or password, or not an administrator."));
        };
        self.login_throttle.clear(&client);
        match self.sessions.create(&user.name) {
            Ok(id) => Response::redirect("/")
                .with_header(
                    "Set-Cookie",
                    self.cookie(SESSION_COOKIE, &id, self.config.session_seconds),
                )
                .with_header("Set-Cookie", self.cookie(LOGIN_COOKIE, "", 0)),
            Err(_) => Response::text(500, "unable to start a session"),
        }
    }

    fn logout(&self, req: &Request) -> Response {
        if let Some((id, _)) = self.session(req) {
            if !Self::same_origin(req)
                || !self.sessions.verify_csrf(&id, req.form().get("csrf_token").unwrap_or(""))
            {
                return Response::text(403, "invalid or missing CSRF token");
            }
            self.sessions.end(&id);
        }
        Response::redirect("/login").with_header("Set-Cookie", self.cookie(SESSION_COOKIE, "", 0))
    }

    fn status_page(&self, session_id: &str, user: &str) -> Response {
        let api = &self.api;
        let lib = api.library.library();
        let scan = api.library.status();
        let mut body = String::from("<h1>Status</h1>\n");
        let _ = writeln!(body, "<p class=\"page-intro\">Signed in as {}.</p>", esc(user));

        body.push_str(&card("library", "Library"));
        let _ = write!(
            body,
            "<dl class=\"details-list\">\n<dt>Tracks</dt><dd>{}</dd>\n<dt>Albums</dt><dd>{}</dd>\n\
             <dt>Artists</dt><dd>{}</dd>\n<dt>Genres</dt><dd>{}</dd>\n<dt>Folders</dt><dd>{}</dd>\n\
             <dt>Total playing time</dt><dd>{}</dd>\n<dt>Music directory</dt><dd><code>{}</code></dd>\n</dl>\n</section>\n",
            lib.tracks.len(),
            lib.albums.len(),
            lib.artists_sorted.len(),
            lib.genres.len(),
            lib.dirs.len(),
            duration(lib.tracks.iter().map(|t| t.rec.duration_ms).sum()),
            esc(&self.config.music_dir.display().to_string()),
        );

        body.push_str(&card("scan", "Scanning"));
        let state = if scan.scanning {
            format!("Scanning now: {} files seen, {} read", scan.files_seen, scan.files_probed)
        } else {
            "Idle".to_owned()
        };
        let interval = self.config.scan_interval.map_or("only at startup".to_owned(), |d| {
            format!("every {} minutes", d.as_secs() / 60)
        });
        let _ = write!(
            body,
            "<dl class=\"details-list\">\n<dt>State</dt><dd>{}</dd>\n<dt>Rescans</dt><dd>{}</dd>\n\
             <dt>Last finished</dt><dd>{}</dd>\n<dt>Took</dt><dd>{}</dd>\n<dt>Files read last time</dt><dd>{}</dd>\n\
             <dt>Problems</dt><dd>{}</dd>\n</dl>\n",
            esc(&state),
            esc(&interval),
            esc(&when(scan.last_finished_ms)),
            esc(&duration(scan.last_duration_ms)),
            scan.files_probed,
            scan.error_count,
        );
        if let Some(f) = &scan.failure {
            let _ = writeln!(
                body,
                "<p class=\"notice notice-danger\">The last scan failed: {}</p>",
                esc(f)
            );
        }
        if scan.truncated {
            body.push_str("<p class=\"notice notice-warning\">The scan stopped at the track limit (MS_MAX_TRACKS).</p>\n");
        }
        if !scan.errors.is_empty() {
            body.push_str("<div class=\"table-wrap\"><table class=\"data-table\">\n<thead><tr><th>File</th><th>Problem</th></tr></thead>\n<tbody>\n");
            for (path, message) in scan.errors.iter().take(50) {
                let _ = writeln!(
                    body,
                    "<tr><td><code>{}</code></td><td>{}</td></tr>",
                    esc(path),
                    esc(message)
                );
            }
            body.push_str("</tbody></table></div>\n");
        }
        body.push_str("</section>\n");

        body.push_str(&card("users", "Accounts"));
        body.push_str("<div class=\"table-wrap\"><table class=\"data-table\">\n<thead><tr><th>User</th><th>Role</th><th>State</th></tr></thead>\n<tbody>\n");
        for r in &api.roster {
            let _ = writeln!(
                body,
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&r.name),
                if r.admin { "admin" } else { "listener" },
                if r.enabled { "active" } else { "disabled (not in the users file)" }
            );
        }
        body.push_str("</tbody></table></div>\n</section>\n");

        body.push_str(&card("activity", "Activity"));
        let s = &api.stats;
        let enc = api.media.encoders;
        let yes = |b: bool| if b { "yes" } else { "no" };
        let _ = write!(
            body,
            "<dl class=\"details-list\">\n<dt>Transcodes running</dt><dd>{} of {}</dd>\n\
             <dt>Cover resizes running</dt><dd>{} of {}</dd>\n<dt>API calls</dt><dd>{} ({} failed)</dd>\n\
             <dt>Failed logins</dt><dd>{}</dd>\n<dt>Streams started</dt><dd>{} ({} transcoded)</dd>\n\
             <dt>Cover cache</dt><dd>{}</dd>\n<dt>ffmpeg encoders</dt><dd>MP3 {}, Opus {}, JPEG {}</dd>\n\
             <dt>Version</dt><dd>{}</dd>\n<dt>Up for</dt><dd>{}</dd>\n</dl>\n",
            api.media.transcodes.active(),
            api.media.transcodes.max(),
            api.media.resizes.active(),
            api.media.resizes.max(),
            s.calls.load(Ordering::Relaxed),
            s.failed_calls.load(Ordering::Relaxed),
            s.auth_failures.load(Ordering::Relaxed),
            s.streams.load(Ordering::Relaxed),
            s.transcodes.load(Ordering::Relaxed),
            mib(api.covers.total_bytes()),
            yes(enc.mp3),
            yes(enc.opus),
            yes(enc.mjpeg),
            esc(ms_common::VERSION),
            duration(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(0)),
        );
        let unknown = lock(&s.unknown_methods).clone();
        if !unknown.is_empty() {
            body.push_str("<p class=\"field-help\">Calls clients made that this server does not implement:</p>\n<ul>\n");
            for (m, n) in &unknown {
                let _ = writeln!(body, "<li><code>{}</code> ({n})</li>", esc(m));
            }
            body.push_str("</ul>\n");
        }
        body.push_str("</section>\n");

        let page = Page {
            title: "Status".into(),
            nav: "status",
            csrf: Some(self.sessions.csrf_token(session_id)),
            refresh: scan.scanning.then_some(5),
            body,
            ..Page::default()
        };
        Response::html(200, html::render(&page))
    }
}

impl Handler for App {
    fn handle(&self, request: &Request) -> Response {
        self.route(request)
    }

    fn hsts(&self) -> bool {
        self.config.secure_cookies
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(mib(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(when(0), "never");
        assert_eq!(when(1000), "1970-01-01 00:00:01 UTC");
        assert_eq!(duration(1500), "1.5s");
        assert_eq!(duration(61_000), "1m 01s");
        assert_eq!(duration(3_661_000), "1h 01m");
    }
}
