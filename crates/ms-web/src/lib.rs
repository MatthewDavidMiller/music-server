//! The music-server application: configuration, the admin status page,
//! and the assembly of the library, store, media, and Subsonic API into
//! one HTTP handler.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod app;
pub mod config;
pub mod html;
pub mod session;

use std::sync::{Arc, Mutex};

use ms_common::{Error, Result, fsio};
use ms_crypto::MasterKey;
use ms_library::{LibraryConfig, LibraryService};
use ms_media::{Encoders, Media, Slots};
use ms_store::Store;
use ms_store::users::{MAX_USERS_FILE, UserEntry, parse_users_file};
use ms_subsonic::covers::CoverCache;
use ms_subsonic::throttle::Throttle;
use ms_subsonic::{AUTH_FAILURE_LIMIT, AUTH_FAILURE_WINDOW, Api, Credentials, Settings, Stats};

use crate::app::App;
use crate::config::Config;
use crate::session::Sessions;

pub fn load_master_key(config: &Config) -> Result<MasterKey> {
    MasterKey::load(&config.master_key_file).map_err(|_| {
        Error::new(format!(
            "cannot read a 64-hex-digit master key from {} (set MS_MASTER_KEY_FILE)",
            config.master_key_file.display()
        ))
    })
}

/// Reads and validates the users file.
pub fn load_users(config: &Config) -> Result<Vec<UserEntry>> {
    let mut raw = fsio::read_bounded(&config.users_file, MAX_USERS_FILE)
        .map_err(|e| {
            Error::new(format!("cannot read the users file {}: {e}", config.users_file.display()))
        })?
        .ok_or_else(|| {
            Error::new(format!(
                "the users file {} does not exist (set MS_USERS_FILE)",
                config.users_file.display()
            ))
        })?;
    let entries = parse_users_file(&raw);
    ms_common::buf::wipe_vec(&mut raw);
    entries
}

/// Everything `serve` needs, started: the store's flush thread and the
/// library's scan thread are running when this returns.
pub fn build(config: Config) -> Result<App> {
    fsio::mkdir_all(&config.data_dir, 0o700)?;
    fsio::mkdir_all(&config.cache_dir, 0o700)?;
    let master = load_master_key(&config)?;
    let entries = load_users(&config)?;
    let store = Arc::new(Store::open(&config.data_dir, &master)?);
    let (roster, report) = store.sync_roster(&entries)?;
    let log = |what: &str, names: &[String]| {
        if !names.is_empty() {
            eprintln!("music-server: accounts {what}: {}", names.join(", "));
        }
    };
    log("added", &report.added);
    log("disabled (no longer in the users file)", &report.disabled);
    log("enabled again", &report.enabled);
    log("with a changed admin flag", &report.admin_changed);
    store.start_flusher()?;

    let library = Arc::new(LibraryService::new(LibraryConfig {
        music_dir: config.music_dir.clone(),
        cache_dir: config.cache_dir.clone(),
        max_tracks: config.max_tracks,
        interval: config.scan_interval,
    }));
    library.start()?;

    let encoders = match Encoders::probe(&config.ffmpeg) {
        Ok(e) => {
            if !(e.mp3 && e.opus && e.mjpeg) {
                eprintln!(
                    "music-server: ffmpeg lacks some encoders ({e:?}); those features are off"
                );
            }
            e
        }
        Err(e) => {
            eprintln!(
                "music-server: ffmpeg is unavailable, so transcoding and cover resizing are off: {e}"
            );
            Encoders::default()
        }
    };
    let media = Media {
        ffmpeg: config.ffmpeg.clone(),
        encoders,
        transcodes: Slots::new(config.max_transcodes),
        resizes: Slots::new(config.max_cover_resizes),
        scratch: config.tmp_dir.clone(),
    };
    let covers = CoverCache::new(&config.cache_dir.join("covers"), config.cover_cache_bytes)?;
    let web_key = master.derive("web").map_err(|_| Error::new("key derivation failed"))?;
    let api = Api {
        library,
        store,
        media,
        credentials: Credentials::new(entries),
        roster,
        settings: Settings {
            allow_password_auth: config.allow_password_auth,
            default_format: config.default_format,
            default_bitrate: config.transcode_bitrate,
        },
        covers,
        throttle: Throttle::new(AUTH_FAILURE_LIMIT, AUTH_FAILURE_WINDOW),
        now_playing: Mutex::new(Vec::new()),
        stats: Stats::default(),
    };
    let sessions = Sessions::new(web_key, config.session_seconds);
    Ok(App::new(config, api, sessions))
}
