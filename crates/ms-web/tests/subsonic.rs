//! End-to-end tests: the real server on loopback, a generated library on
//! disk, raw HTTP over TCP, and the calls a Subsonic client makes.
//!
//! The ffmpeg-backed parts (transcodes, cover resizing, real audio files)
//! are skipped with a message when ffmpeg is missing, unless
//! MS_REQUIRE_FFMPEG is set, as it is in the toolbox.

#![allow(
    clippy::disallowed_methods,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::format_push_string
)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ms_common::json::{self, Value};
use ms_media::transcode::Format;
use ms_tags::testgen::{Id3Frame, id3v2, mpeg_frames, tiny_png};
use ms_web::app::App;
use ms_web::config::Config;

const USERS: &str = r#"{"version":1,"users":[
  {"name":"admin","password":"admin-password","admin":true},
  {"name":"bob","password":"bob-password"}]}"#;

fn ffmpeg() -> Option<PathBuf> {
    let p = PathBuf::from("/usr/bin/ffmpeg");
    if p.exists() {
        return Some(p);
    }
    assert!(
        std::env::var_os("MS_REQUIRE_FFMPEG").is_none(),
        "MS_REQUIRE_FFMPEG is set but ffmpeg is missing"
    );
    eprintln!("skipping the ffmpeg parts: ffmpeg is not installed");
    None
}

fn mp3(title: &str, artist: &str, album: &str, track: &str) -> Vec<u8> {
    let mut f = id3v2(
        3,
        &[
            Id3Frame::text("TIT2", title),
            Id3Frame::text("TPE1", artist),
            Id3Frame::text("TALB", album),
            Id3Frame::text("TRCK", track),
            Id3Frame::text("TYER", "1999"),
            Id3Frame::text("TCON", "Rock"),
        ],
        false,
    );
    f.extend(mpeg_frames(200, false));
    f
}

struct Server {
    addr: SocketAddr,
    dir: PathBuf,
    app: Arc<App>,
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The `subsonic-response` object of a JSON reply.
    fn json(&self) -> Value {
        let v = json::parse(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.text()));
        v.get("subsonic-response").cloned().unwrap()
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie")).find_map(
            |(_, v)| {
                let pair = v.split(';').next()?;
                pair.starts_with(&format!("{name}=")).then(|| pair.to_owned())
            },
        )
    }
}

fn config(dir: &Path, addr: SocketAddr) -> Config {
    Config {
        listen: addr,
        music_dir: dir.join("music"),
        data_dir: dir.join("data"),
        cache_dir: dir.join("cache"),
        master_key_file: dir.join("key"),
        users_file: dir.join("users.json"),
        trusted_proxies: Vec::new(),
        secure_cookies: false,
        session_seconds: 3600,
        scan_interval: None,
        max_tracks: 10_000,
        ffmpeg: PathBuf::from("/usr/bin/ffmpeg"),
        max_transcodes: 1,
        max_cover_resizes: 1,
        tmp_dir: dir.join("tmp"),
        default_format: Format::Mp3,
        transcode_bitrate: 128,
        cover_cache_bytes: 16 * 1024 * 1024,
        allow_password_auth: true,
    }
}

impl Server {
    fn prepare(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ms-web-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let music = dir.join("music");
        std::fs::create_dir_all(music.join("The Band/First Album")).unwrap();
        std::fs::create_dir_all(music.join("Solo/Debut")).unwrap();
        std::fs::create_dir_all(dir.join("tmp")).unwrap();
        std::fs::write(
            music.join("The Band/First Album/01 Opening.mp3"),
            mp3("Opening", "The Band", "First Album", "1"),
        )
        .unwrap();
        std::fs::write(
            music.join("The Band/First Album/02 Closing.mp3"),
            mp3("Closing", "The Band", "First Album", "2"),
        )
        .unwrap();
        std::fs::write(music.join("The Band/First Album/cover.png"), tiny_png(10, 10)).unwrap();
        std::fs::write(
            music.join("Solo/Debut/01 Hello.mp3"),
            mp3("Hello World", "Solo", "Debut", "1"),
        )
        .unwrap();
        std::fs::write(dir.join("key"), "7".repeat(64)).unwrap();
        std::fs::write(dir.join("users.json"), USERS).unwrap();
        dir
    }

    fn start(dir: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Arc::new(ms_web::build(config(dir, addr)).unwrap());
        let served = Arc::clone(&app);
        thread::spawn(move || ms_http::serve(&listener, &served));
        let s = Self { addr, dir: dir.to_path_buf(), app };
        s.wait_for_scan(1);
        s
    }

    fn wait_for_scan(&self, scans: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.app.api.library.status().scans_completed < scans {
            assert!(Instant::now() < deadline, "the scan did not finish");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn raw(&self, request: &str, body: &[u8]) -> Reply {
        let mut stream = TcpStream::connect(self.addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        let end = out.windows(4).position(|w| w == b"\r\n\r\n").expect("no header end") + 4;
        let head = String::from_utf8(out[..end].to_vec()).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap()[9..12].parse().unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        Reply { status, headers, body: out[end..].to_vec() }
    }

    fn get(&self, path: &str, extra: &[(&str, &str)]) -> Reply {
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: {}\r\n", self.addr);
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        self.raw(&req, b"")
    }

    fn post(&self, path: &str, form: &str, cookies: &[&str]) -> Reply {
        let mut req = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            self.addr,
            form.len()
        );
        if !cookies.is_empty() {
            req.push_str(&format!("Cookie: {}\r\n", cookies.join("; ")));
        }
        req.push_str("\r\n");
        self.raw(&req, form.as_bytes())
    }

    /// A Subsonic call as `user`, with token authentication, in JSON.
    fn api(&self, user: &str, method: &str, query: &str) -> Reply {
        let password = if user == "admin" { "admin-password" } else { "bob-password" };
        let salt = ms_common::rand::random_hex(6).unwrap();
        let token =
            ms_crypto::hex(&ms_crypto::md5(format!("{password}{salt}").as_bytes()).unwrap());
        let sep = if query.is_empty() { "" } else { "&" };
        self.get(
            &format!(
                "/rest/{method}.view?u={user}&t={token}&s={salt}&v=1.16.1&c=test&f=json{sep}{query}"
            ),
            &[],
        )
    }

    fn ok(&self, user: &str, method: &str, query: &str) -> Value {
        let r = self.api(user, method, query);
        assert_eq!(r.status, 200);
        let v = r.json();
        assert_eq!(v.get("status").and_then(Value::as_str), Some("ok"), "{method}: {}", r.text());
        v
    }

    fn error_code(&self, user: &str, method: &str, query: &str) -> u64 {
        let v = self.api(user, method, query).json();
        assert_eq!(v.get("status").and_then(Value::as_str), Some("failed"));
        v.get("error").and_then(|e| e.get("code")).and_then(Value::as_u64).unwrap()
    }
}

fn arr<'a>(v: &'a Value, path: &[&str]) -> &'a [Value] {
    let mut cur = v;
    for p in path {
        cur = cur.get(p).unwrap_or_else(|| panic!("no {p} in {cur:?}"));
    }
    cur.as_array().unwrap()
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("no {key} in {v:?}"))
}

#[test]
fn authentication() {
    let dir = Server::prepare("auth");
    let srv = Server::start(&dir);
    srv.ok("admin", "ping", "");
    // XML is the default.
    let xml = srv.get("/rest/ping?u=bob&p=bob-password&v=1.16.1&c=test", &[]);
    assert!(
        xml.text()
            .contains("<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\""),
        "{}",
        xml.text()
    );
    assert!(xml.header("content-type").unwrap().starts_with("text/xml"));
    let enc = ms_crypto::hex(b"bob-password");
    let r = srv.get(&format!("/rest/ping.view?u=bob&p=enc:{enc}&v=1.16.1&c=t&f=json"), &[]);
    assert_eq!(s(&r.json(), "status"), "ok");
    // Form-encoded POST (OpenSubsonic formPost).
    let r = srv.post("/rest/ping.view", "u=bob&p=bob-password&v=1.16.1&c=t&f=json", &[]);
    assert_eq!(s(&r.json(), "status"), "ok");
    // Failures, then the throttle.
    for _ in 0..10 {
        let r = srv.get("/rest/ping.view?u=bob&p=wrong-password&v=1.16.1&c=t&f=json", &[]);
        assert_eq!(r.json().get("error").unwrap().get("code").unwrap().as_u64(), Some(40));
    }
    let r = srv.get("/rest/ping.view?u=bob&p=bob-password&v=1.16.1&c=t&f=json", &[]);
    assert!(s(r.json().get("error").unwrap(), "message").contains("Too many"));
    cleanup(&dir);
}

#[test]
fn browsing_lists_and_search() {
    let dir = Server::prepare("browse");
    let srv = Server::start(&dir);
    let artists = srv.ok("bob", "getArtists", "");
    let index = arr(&artists, &["artists", "index"]);
    let letters: Vec<&str> = index.iter().map(|i| s(i, "name")).collect();
    assert_eq!(letters, ["B", "S"]);
    let band = &arr(&index[0], &["artist"])[0];
    assert_eq!(s(band, "name"), "The Band");
    let artist = srv.ok("bob", "getArtist", &format!("id={}", s(band, "id")));
    let album = &arr(&artist, &["artist", "album"])[0];
    assert_eq!(s(album, "name"), "First Album");
    assert!(album.get("coverArt").is_some());
    let album = srv.ok("bob", "getAlbum", &format!("id={}", s(album, "id")));
    let songs = arr(&album, &["album", "song"]);
    assert_eq!(songs.iter().map(|x| s(x, "title")).collect::<Vec<_>>(), ["Opening", "Closing"]);
    assert_eq!(s(&songs[0], "contentType"), "audio/mpeg");
    assert!(songs[0].get("duration").unwrap().as_u64().unwrap() >= 5);
    let song_id = s(&songs[0], "id").to_owned();
    let song = srv.ok("bob", "getSong", &format!("id={song_id}"));
    assert_eq!(s(song.get("song").unwrap(), "album"), "First Album");

    // Folder view.
    let indexes = srv.ok("bob", "getIndexes", "");
    let dirs: Vec<&str> = arr(&indexes, &["indexes", "index"])
        .iter()
        .flat_map(|i| arr(i, &["artist"]).iter().map(|a| s(a, "name")))
        .collect();
    // "The Band" files under B: the article is ignored.
    assert_eq!(dirs, ["The Band", "Solo"]);
    let solo = arr(&indexes, &["indexes", "index"])[1].get("artist").unwrap().as_array().unwrap()
        [0]
    .clone();
    let d = srv.ok("bob", "getMusicDirectory", &format!("id={}", s(&solo, "id")));
    let children = arr(&d, &["directory", "child"]);
    assert_eq!(s(&children[0], "title"), "Debut");
    assert_eq!(children[0].get("isDir").unwrap().as_bool(), Some(true));

    // Lists and search.
    for kind in [
        "newest",
        "random",
        "alphabeticalByName",
        "alphabeticalByArtist",
        "frequent",
        "recent",
        "starred",
        "highest",
    ] {
        srv.ok("bob", "getAlbumList2", &format!("type={kind}"));
        srv.ok("bob", "getAlbumList", &format!("type={kind}"));
    }
    let by_year = srv.ok("bob", "getAlbumList2", "type=byYear&fromYear=1990&toYear=2000");
    assert_eq!(arr(&by_year, &["albumList2", "album"]).len(), 2);
    let by_genre = srv.ok("bob", "getAlbumList2", "type=byGenre&genre=rock");
    assert_eq!(arr(&by_genre, &["albumList2", "album"]).len(), 2);
    assert_eq!(srv.error_code("bob", "getAlbumList2", "type=nonsense"), 10);
    let random = srv.ok("bob", "getRandomSongs", "size=2");
    assert_eq!(arr(&random, &["randomSongs", "song"]).len(), 2);
    let found = srv.ok("bob", "search3", "query=hello");
    assert_eq!(s(&arr(&found, &["searchResult3", "song"])[0], "title"), "Hello World");
    let everything = srv.ok("bob", "search3", "query=%22%22&songCount=100");
    assert_eq!(arr(&everything, &["searchResult3", "song"]).len(), 3);
    let genres = srv.ok("bob", "getGenres", "");
    assert_eq!(s(&arr(&genres, &["genres", "genre"])[0], "value"), "Rock");
    srv.ok("bob", "getMusicFolders", "");
    srv.ok("bob", "getLicense", "");
    let ext = srv.ok("bob", "getOpenSubsonicExtensions", "");
    assert!(ext.get("openSubsonicExtensions").unwrap().as_array().is_some());
    let status = srv.ok("bob", "getScanStatus", "");
    assert_eq!(status.get("scanStatus").unwrap().get("count").unwrap().as_u64(), Some(3));
    srv.ok("bob", "getPodcasts", "");
    assert_eq!(srv.error_code("bob", "getAlbum", "id=al-000000000000000000000000"), 70);
    assert_eq!(srv.error_code("bob", "doesNotExist", ""), 0);
    assert_eq!(srv.error_code("bob", "startScan", ""), 50);
    srv.ok("admin", "startScan", "");
    cleanup(&dir);
}

#[test]
fn streaming_ranges_and_covers() {
    let dir = Server::prepare("stream");
    let srv = Server::start(&dir);
    let found = srv.ok("bob", "search3", "query=opening");
    let song = &arr(&found, &["searchResult3", "song"])[0];
    let id = s(song, "id");
    let size = song.get("size").unwrap().as_u64().unwrap();
    let auth = "u=bob&p=bob-password&v=1.16.1&c=test";
    let whole = srv.get(&format!("/rest/stream.view?{auth}&id={id}"), &[]);
    assert_eq!(whole.status, 200);
    assert_eq!(whole.body.len() as u64, size);
    assert_eq!(whole.header("accept-ranges"), Some("bytes"));
    let part = srv.get(&format!("/rest/stream.view?{auth}&id={id}"), &[("Range", "bytes=10-19")]);
    assert_eq!(part.status, 206);
    assert_eq!(part.body, whole.body[10..20]);
    assert_eq!(part.header("content-range"), Some(format!("bytes 10-19/{size}").as_str()));
    let beyond = srv
        .get(&format!("/rest/stream.view?{auth}&id={id}"), &[("Range", &format!("bytes={size}-"))]);
    assert_eq!(beyond.status, 416);
    let head =
        srv.raw(&format!("HEAD /rest/stream.view?{auth}&id={id} HTTP/1.1\r\nHost: x\r\n\r\n"), b"");
    assert!(head.body.is_empty() && head.header("content-length") == Some(&size.to_string()));
    let dl = srv.get(&format!("/rest/download.view?{auth}&id={id}"), &[]);
    assert!(dl.header("content-disposition").unwrap().contains("01%20Opening.mp3"));
    // A bitrate ceiling the file already meets streams the original.
    let under = srv.get(&format!("/rest/stream.view?{auth}&id={id}&maxBitRate=320"), &[]);
    assert_eq!(under.body.len() as u64, size);
    // Cover art: the folder image, as-is when small enough.
    let album = s(song, "albumId");
    let cover = srv.get(&format!("/rest/getCoverArt.view?{auth}&id={album}"), &[]);
    assert_eq!(cover.status, 200);
    assert_eq!(cover.header("content-type"), Some("image/png"));
    let etag = cover.header("etag").unwrap().to_owned();
    let again =
        srv.get(&format!("/rest/getCoverArt.view?{auth}&id={album}"), &[("If-None-Match", &etag)]);
    assert_eq!(again.status, 304);
    let small = srv.get(&format!("/rest/getCoverArt.view?{auth}&id={album}&size=300"), &[]);
    assert_eq!(small.header("content-type"), Some("image/png"));
    let none = srv.api("bob", "getCoverArt", "id=al-000000000000000000000000");
    assert_eq!(none.json().get("error").unwrap().get("code").unwrap().as_u64(), Some(70));

    if let Some(ff) = ffmpeg() {
        // Real audio for the transcoder, and a large cover to resize.
        let music = dir.join("music/Real");
        std::fs::create_dir_all(&music).unwrap();
        let run = |args: &[&str]| {
            let st = Command::new(&ff)
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(args)
                .status()
                .unwrap();
            assert!(st.success());
        };
        let flac = music.join("tone.flac");
        run(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
            "-metadata",
            "title=Tone",
            "-metadata",
            "album=Real",
            flac.to_str().unwrap(),
        ]);
        run(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=s=800x800",
            "-frames:v",
            "1",
            music.join("folder.jpg").to_str().unwrap(),
        ]);
        // A WAV is 1411 kbit/s, over any phone's ceiling.
        let wav = music.join("wave.wav");
        run(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=220:duration=2",
            "-metadata",
            "title=Wave",
            wav.to_str().unwrap(),
        ]);
        srv.ok("admin", "startScan", "");
        srv.wait_for_scan(2);
        let found = srv.ok("bob", "search3", "query=tone");
        let tone = &arr(&found, &["searchResult3", "song"])[0];
        let tone_id = s(tone, "id");
        let mp3 = srv
            .get(&format!("/rest/stream.view?{auth}&id={tone_id}&format=mp3&maxBitRate=96"), &[]);
        assert_eq!(mp3.header("content-type"), Some("audio/mpeg"));
        assert!(mp3.header("content-length").is_none());
        assert!(mp3.body.len() > 20_000 && mp3.body[0] == 0xff, "{} bytes", mp3.body.len());
        let opus = srv.get(
            &format!(
                "/rest/stream.view?{auth}&id={tone_id}&format=opus&estimateContentLength=true"
            ),
            &[],
        );
        assert_eq!(opus.header("content-type"), Some("audio/ogg"));
        let promised: usize = opus.header("content-length").unwrap().parse().unwrap();
        assert_eq!(opus.body.len(), promised);
        assert!(opus.body.starts_with(b"OggS"));
        let raw = srv
            .get(&format!("/rest/stream.view?{auth}&id={tone_id}&format=raw&maxBitRate=64"), &[]);
        assert_eq!(raw.header("content-type"), Some("audio/flac"));
        // Only a ceiling, no format: the default format (MP3) at that rate.
        let found = srv.ok("bob", "search3", "query=wave");
        let wave_id = s(&arr(&found, &["searchResult3", "song"])[0], "id").to_owned();
        let capped = srv.get(&format!("/rest/stream.view?{auth}&id={wave_id}&maxBitRate=64"), &[]);
        assert_eq!(capped.header("content-type"), Some("audio/mpeg"));
        assert!(capped.body.len() < 40_000, "{} bytes at 64 kbit/s", capped.body.len());
        let uncapped = srv.get(&format!("/rest/stream.view?{auth}&id={wave_id}"), &[]);
        assert_eq!(uncapped.header("content-type"), Some("audio/wav"));
        assert_eq!(srv.app.api.media.transcodes.active(), 0);
        let album = s(tone, "albumId");
        let resized = srv.get(&format!("/rest/getCoverArt.view?{auth}&id={album}&size=100"), &[]);
        assert_eq!(resized.header("content-type"), Some("image/jpeg"));
        assert_eq!(ms_tags::image::dimensions(&resized.body), Some((128, 128)));
        let cached = srv.get(&format!("/rest/getCoverArt.view?{auth}&id={album}&size=128"), &[]);
        assert_eq!(cached.body, resized.body);
        assert!(srv.app.api.covers.total_bytes() > 0);
    }
    cleanup(&dir);
}

#[test]
fn playlists_stars_plays_and_restarts() {
    let dir = Server::prepare("playlists");
    let srv = Server::start(&dir);
    let all = srv.ok("bob", "search3", "query=&songCount=10");
    let ids: Vec<String> =
        arr(&all, &["searchResult3", "song"]).iter().map(|x| s(x, "id").to_owned()).collect();
    let created = srv.ok(
        "bob",
        "createPlaylist",
        &format!("name=Road%20Trip&songId={}&songId={}", ids[0], ids[1]),
    );
    let pl = created.get("playlist").unwrap();
    let pl_id = s(pl, "id").to_owned();
    assert_eq!(pl.get("songCount").unwrap().as_u64(), Some(2));
    srv.ok(
        "bob",
        "updatePlaylist",
        &format!(
            "playlistId={pl_id}&songIndexToRemove=0&songIdToAdd={}&name=Renamed&public=true",
            ids[2]
        ),
    );
    let got = srv.ok("bob", "getPlaylist", &format!("id={pl_id}"));
    let entries: Vec<&str> = arr(&got, &["playlist", "entry"]).iter().map(|e| s(e, "id")).collect();
    assert_eq!(entries, [ids[1].as_str(), ids[2].as_str()]);
    assert_eq!(s(got.get("playlist").unwrap(), "name"), "Renamed");
    // Public: admin sees it; bob's private one would not be.
    let lists = srv.ok("admin", "getPlaylists", "");
    assert_eq!(arr(&lists, &["playlists", "playlist"]).len(), 1);
    assert_eq!(
        srv.error_code("bob", "createPlaylist", "name=x&songId=tr-000000000000000000000000"),
        70
    );
    // Stars and plays.
    srv.ok(
        "bob",
        "star",
        &format!(
            "id={}&albumId={}",
            ids[0],
            s(&arr(&all, &["searchResult3", "song"])[0], "albumId")
        ),
    );
    let starred = srv.ok("bob", "getStarred2", "");
    assert_eq!(arr(&starred, &["starred2", "song"]).len(), 1);
    assert_eq!(arr(&starred, &["starred2", "album"]).len(), 1);
    srv.ok("bob", "scrobble", &format!("id={}&submission=true", ids[0]));
    srv.ok("bob", "scrobble", &format!("id={}&submission=false", ids[1]));
    let now = srv.ok("bob", "getNowPlaying", "");
    assert_eq!(s(&arr(&now, &["nowPlaying", "entry"])[0], "id"), ids[1]);
    srv.app.api.store.flush().unwrap();
    let song = srv.ok("bob", "getSong", &format!("id={}", ids[0]));
    assert_eq!(song.get("song").unwrap().get("playCount").unwrap().as_u64(), Some(1));
    assert!(song.get("song").unwrap().get("starred").is_some());
    let frequent = srv.ok("bob", "getAlbumList2", "type=frequent");
    assert_eq!(arr(&frequent, &["albumList2", "album"]).len(), 1);
    // Another user's data is separate.
    let admin_song = srv.ok("admin", "getSong", &format!("id={}", ids[0]));
    assert!(admin_song.get("song").unwrap().get("playCount").is_none());
    srv.ok("bob", "unstar", &format!("id={}", ids[0]));
    // Users: bob can see himself, not others.
    srv.ok("bob", "getUser", "username=bob");
    assert_eq!(srv.error_code("bob", "getUser", "username=admin"), 50);
    assert_eq!(arr(&srv.ok("admin", "getUsers", ""), &["users", "user"]).len(), 2);
    // Deleting is the owner's (or an admin's) right.
    let theirs = srv.ok("admin", "createPlaylist", "name=Admin%20only");
    let theirs_id = s(theirs.get("playlist").unwrap(), "id").to_owned();
    assert_eq!(srv.error_code("bob", "deletePlaylist", &format!("id={theirs_id}")), 70);

    // A restart keeps everything; removing bob from the users file
    // disables him but keeps his data, and adding him back restores it.
    drop(srv);
    std::fs::write(
        dir.join("users.json"),
        r#"{"version":1,"users":[{"name":"admin","password":"admin-password","admin":true}]}"#,
    )
    .unwrap();
    let srv = Server::start(&dir);
    assert_eq!(srv.error_code("bob", "ping", ""), 40);
    let roster = &srv.app.api.roster;
    assert!(roster.iter().any(|r| r.name == "bob" && !r.enabled));
    drop(srv);
    std::fs::write(dir.join("users.json"), USERS).unwrap();
    let srv = Server::start(&dir);
    let got = srv.ok("bob", "getPlaylist", &format!("id={pl_id}"));
    assert_eq!(arr(&got, &["playlist", "entry"]).len(), 2);
    srv.ok("bob", "deletePlaylist", &format!("id={pl_id}"));
    assert_eq!(srv.error_code("bob", "getPlaylist", &format!("id={pl_id}")), 70);
    // Nothing in the data directory is plaintext.
    for entry in walk(&dir.join("data")) {
        let raw = std::fs::read(&entry).unwrap();
        assert!(raw.starts_with(b"MSF1"), "{}", entry.display());
        assert!(!raw.windows(5).any(|w| w == b"Admin"), "{}", entry.display());
    }
    cleanup(&dir);
}

/// Removes a test's directory. A scan thread may still be writing the
/// cache, so a failure is retried briefly and then ignored.
fn cleanup(dir: &Path) {
    for _ in 0..50 {
        if std::fs::remove_dir_all(dir).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

#[test]
fn status_page_is_for_admins() {
    let dir = Server::prepare("status");
    let srv = Server::start(&dir);
    let page = srv.get("/", &[]);
    assert_eq!(page.status, 303);
    assert_eq!(srv.get("/healthz", &[]).status, 200);
    let login = |user: &str, password: &str| {
        let form = srv.get("/login", &[]);
        let nonce = form.cookie("ms_login").unwrap();
        let text = form.text();
        let marker = "name=\"csrf_token\" value=\"";
        let start = text.find(marker).unwrap() + marker.len();
        let token = text[start..].split('"').next().unwrap().to_owned();
        srv.post(
            "/login",
            &format!("csrf_token={token}&username={user}&password={password}"),
            &[&nonce],
        )
    };
    let refused = login("bob", "bob-password");
    assert_eq!(refused.status, 401);
    let wrong = login("admin", "nope");
    assert_eq!(wrong.status, 401);
    let ok = login("admin", "admin-password");
    assert_eq!(ok.status, 303);
    let session = ok.cookie("ms_session").unwrap();
    let status = srv.get("/", &[("Cookie", &session)]);
    assert_eq!(status.status, 200);
    let text = status.text();
    assert!(text.contains("<dt>Tracks</dt><dd>3</dd>"), "{text}");
    assert!(text.contains("bob"));
    assert!(status.header("content-security-policy").unwrap().contains("default-src 'none'"));
    // Logging out needs the CSRF token.
    assert_eq!(srv.post("/logout", "csrf_token=bad", &[&session]).status, 403);
    let marker = "name=\"csrf_token\" value=\"";
    let start = text.find(marker).unwrap() + marker.len();
    let csrf = text[start..].split('"').next().unwrap();
    assert_eq!(srv.post("/logout", &format!("csrf_token={csrf}"), &[&session]).status, 303);
    assert_eq!(srv.get("/", &[("Cookie", &session)]).status, 303);
    assert_eq!(srv.get("/nothing", &[]).status, 404);
    cleanup(&srv.dir);
}
