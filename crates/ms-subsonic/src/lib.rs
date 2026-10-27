//! The Subsonic API (1.16.1, with the OpenSubsonic markers clients look
//! for), served under `/rest/<method>` and `/rest/<method>.view`.
//!
//! A call is authenticated, dispatched by method name, and answered in XML
//! or, with `f=json`, JSON. Media calls (`stream`, `download`,
//! `getCoverArt`) answer with the bytes themselves; their failures are
//! ordinary Subsonic error documents.
//!
//! Unknown methods answer with a generic error and are logged by name only
//! (never with their parameters, which carry credentials), so a client that
//! needs something missing shows up in the log.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod auth;
mod browse;
pub mod covers;
pub mod encode;
pub mod error;
mod lists;
mod media;
pub mod model;
pub mod params;
mod playlists;
mod system;
pub mod throttle;
mod user;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ms_http::{Request, Response};
use ms_library::LibraryService;
use ms_media::Media;
use ms_media::transcode::Format as AudioFormat;
use ms_store::{RosterUser, Store, lock};

pub use auth::{Credentials, User};
use encode::{Format, Node, render};
use error::{ApiError, ApiResult};
use params::Params;
use throttle::Throttle;

/// Failed authentications a client may make inside the window.
pub const AUTH_FAILURE_LIMIT: usize = 10;
pub const AUTH_FAILURE_WINDOW: Duration = Duration::from_mins(15);
const MAX_NOW_PLAYING: usize = 64;
const MAX_UNKNOWN_METHODS: usize = 64;

#[derive(Clone, Debug)]
pub struct Settings {
    pub allow_password_auth: bool,
    pub default_format: AudioFormat,
    pub default_bitrate: u32,
}

/// What someone is playing, for `getNowPlaying`.
#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub user: String,
    pub track: ms_library::Id,
    pub client: String,
    pub at_ms: u64,
}

/// Counters for the status page.
#[derive(Debug, Default)]
pub struct Stats {
    pub calls: AtomicU64,
    pub failed_calls: AtomicU64,
    pub auth_failures: AtomicU64,
    pub streams: AtomicU64,
    pub transcodes: AtomicU64,
    pub unknown_methods: Mutex<BTreeMap<String, u64>>,
}

#[derive(Debug)]
pub struct Api {
    pub library: Arc<LibraryService>,
    pub store: Arc<Store>,
    pub media: Media,
    pub credentials: Credentials,
    pub roster: Vec<RosterUser>,
    pub settings: Settings,
    pub covers: covers::CoverCache,
    pub throttle: Throttle,
    pub now_playing: Mutex<Vec<NowPlaying>>,
    pub stats: Stats,
}

/// What a handler produces.
pub(crate) enum Reply {
    Ok,
    Node(Node),
    Raw(Response),
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reply::Ok => f.write_str("Ok"),
            Reply::Node(n) => write!(f, "Node({})", n.name),
            Reply::Raw(r) => write!(f, "Raw({})", r.status),
        }
    }
}

/// The method name of a `/rest/...` path.
pub fn method_name(path: &str) -> Option<&str> {
    let m = path.strip_prefix("/rest/")?;
    let m = m.strip_suffix(".view").unwrap_or(m);
    (!m.is_empty() && m.len() <= 64 && m.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(m)
}

impl Api {
    fn document(format: Format, payload: Option<Node>, ok: bool) -> Response {
        Response::new(200, format.content_type(), render(format, payload, ok))
    }

    fn failure(&self, format: Format, e: &ApiError) -> Response {
        self.stats.failed_calls.fetch_add(1, Ordering::Relaxed);
        Self::document(format, Some(e.node()), false)
    }

    /// Answers one `/rest/...` request. `client` is the throttling key.
    pub fn handle(&self, req: &Request, client: &str) -> Response {
        self.stats.calls.fetch_add(1, Ordering::Relaxed);
        let form = req.params();
        let format = match form.get("f") {
            Some("json") => Format::Json,
            Some("jsonp") => {
                return self.failure(Format::Json, &ApiError::generic("JSONP is not supported"));
            }
            _ => Format::Xml,
        };
        let Some(method) = method_name(&req.path) else {
            return self.failure(format, &ApiError::generic("Unknown method"));
        };
        if let Some(wait) = self.throttle.locked_out(client) {
            let e = ApiError::new(
                40,
                format!("Too many failed attempts; try again in {} minute(s)", wait.div_ceil(60)),
            );
            return self.failure(format, &e);
        }
        let user = match self.credentials.authenticate(&form, self.settings.allow_password_auth) {
            Ok(u) => u,
            Err(e) => {
                if e.code == 40 {
                    self.throttle.record_failure(client);
                    self.stats.auth_failures.fetch_add(1, Ordering::Relaxed);
                    eprintln!("music-server: failed Subsonic login from {client}");
                }
                return self.failure(format, &e);
            }
        };
        let client_name = form.get("c").unwrap_or("").chars().take(64).collect::<String>();
        let p = Params(&form);
        match self.dispatch(method, &p, &user, req, &client_name) {
            Ok(Reply::Ok) => Self::document(format, None, true),
            Ok(Reply::Node(n)) => Self::document(format, Some(n), true),
            Ok(Reply::Raw(r)) => r,
            Err(e) => self.failure(format, &e),
        }
    }

    fn dispatch(
        &self,
        method: &str,
        p: &Params<'_>,
        user: &User,
        req: &Request,
        client: &str,
    ) -> ApiResult<Reply> {
        match method {
            "ping" | "setRating" | "savePlayQueue" => Ok(Reply::Ok),
            "getLicense" => Ok(Reply::Node(system::license())),
            "getOpenSubsonicExtensions" => Ok(Reply::Node(system::extensions())),
            "getScanStatus" => Ok(Reply::Node(self.scan_status())),
            "startScan" => self.start_scan(user),
            "getMusicFolders" => Ok(Reply::Node(system::music_folders())),
            "getIndexes" => self.get_indexes(p, user),
            "getMusicDirectory" => self.get_music_directory(p, user),
            "getArtists" => self.get_artists(user),
            "getArtist" => self.get_artist(p, user),
            "getAlbum" => self.get_album(p, user),
            "getSong" => self.get_song(p, user),
            "getGenres" => Ok(Reply::Node(self.get_genres())),
            "getAlbumList" => self.get_album_list(p, user, false),
            "getAlbumList2" => self.get_album_list(p, user, true),
            "getRandomSongs" => self.get_random_songs(p, user),
            "getSongsByGenre" => self.get_songs_by_genre(p, user),
            "getStarred" => self.get_starred(user, false),
            "getStarred2" => self.get_starred(user, true),
            "search2" => self.search(p, user, false),
            "search3" => self.search(p, user, true),
            "getPlaylists" => self.get_playlists(p, user),
            "getPlaylist" => self.get_playlist(p, user),
            "createPlaylist" => self.create_playlist(p, user),
            "updatePlaylist" => self.update_playlist(p, user),
            "deletePlaylist" => self.delete_playlist(p, user),
            "scrobble" => self.scrobble(p, user, client),
            "star" => self.star(p, user, true),
            "unstar" => self.star(p, user, false),
            "getUser" => self.get_user(p, user),
            "getUsers" => self.get_users(user),
            "getNowPlaying" => self.get_now_playing(user),
            "stream" => self.stream(p, user, req, client),
            "download" => self.download(p, req),
            "getCoverArt" => self.get_cover_art(p, req, user),
            other => {
                if let Some(reply) = system::stub(other) {
                    Ok(reply)
                } else {
                    self.note_unknown(other);
                    Err(ApiError::generic(format!("Unknown method: {other}")))
                }
            }
        }
    }

    fn note_unknown(&self, method: &str) {
        let mut seen = lock(&self.stats.unknown_methods);
        if !seen.contains_key(method) {
            eprintln!("music-server: unknown subsonic method {method}");
        }
        if seen.len() < MAX_UNKNOWN_METHODS || seen.contains_key(method) {
            *seen.entry(method.to_owned()).or_insert(0) += 1;
        }
    }

    pub(crate) fn set_now_playing(&self, user: &str, track: ms_library::Id, client: &str) {
        let mut list = lock(&self.now_playing);
        list.retain(|n| !(n.user == user && n.client == client));
        list.push(NowPlaying {
            user: user.to_owned(),
            track,
            client: client.to_owned(),
            at_ms: ms_common::timefmt::now_ms(),
        });
        if list.len() > MAX_NOW_PLAYING {
            list.remove(0);
        }
    }

    /// Runs `f` over the current library and the caller's own data (plays
    /// and stars decorate every element). The caller's data stays locked
    /// meanwhile; other users are unaffected.
    pub(crate) fn with_view<R>(
        &self,
        user: &User,
        f: impl FnOnce(&model::View<'_>) -> ApiResult<R>,
    ) -> ApiResult<R> {
        let lib = self.library.library();
        let cell = self.store.user(&user.name)?;
        let ud = lock(&cell);
        f(&model::View { lib: &lib, ud: &ud })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names() {
        assert_eq!(method_name("/rest/ping.view"), Some("ping"));
        assert_eq!(method_name("/rest/getAlbumList2"), Some("getAlbumList2"));
        assert_eq!(method_name("/rest/"), None);
        assert_eq!(method_name("/rest/../x"), None);
        assert_eq!(method_name("/other/ping"), None);
    }
}
