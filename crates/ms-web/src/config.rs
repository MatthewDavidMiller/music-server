//! Runtime configuration, read once from the environment at startup. A value
//! that is set but invalid stops the server rather than being replaced by a
//! default.
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `MS_LISTEN` | `127.0.0.1:8080` | Address the HTTP listener binds |
//! | `MS_MUSIC_DIR` | `/music` | The library, read-only |
//! | `MS_DATA_DIR` | `/var/lib/music-server` | Sealed roster and user data (back this up) |
//! | `MS_CACHE_DIR` | `/var/cache/music-server` | Library index and resized covers (rebuildable) |
//! | `MS_MASTER_KEY_FILE` | `/run/secrets/music_server_master_key` | 64 hex digits |
//! | `MS_USERS_FILE` | `/run/secrets/music_server_users` | The accounts, as JSON |
//! | `MS_TRUSTED_PROXIES` | unset | Comma-separated proxy IPs whose `X-Forwarded-For` is believed |
//! | `MS_SECURE_COOKIES` | `true` | `false` only for plain-HTTP development |
//! | `MS_SESSION_HOURS` | `12` | Status page session lifetime, 1-720 |
//! | `MS_SCAN_INTERVAL_MINUTES` | `60` | Rescan interval; `0` scans only at startup and on request |
//! | `MS_MAX_TRACKS` | `200000` | The most tracks a scan collects |
//! | `MS_FFMPEG` | `/usr/bin/ffmpeg` | The ffmpeg program |
//! | `MS_MAX_TRANSCODES` | `2` | Concurrent transcodes, 0-8; `0` streams originals only |
//! | `MS_MAX_COVER_RESIZES` | `2` | Concurrent cover resizes, 0-8; `0` serves originals |
//! | `MS_TMP_DIR` | `/tmp` | Where embedded covers are staged for a resize (tmpfs) |
//! | `MS_DEFAULT_TRANSCODE_FORMAT` | `mp3` | `mp3` or `opus`, when a client sets only a bitrate |
//! | `MS_TRANSCODE_BITRATE` | `192` | kbit/s, when a client asks for a format without a bitrate |
//! | `MS_COVER_CACHE_MB` | `256` | Resized cover cache ceiling; `0` disables it |
//! | `MS_ALLOW_PASSWORD_AUTH` | `true` | Accept Subsonic `p=` logins besides tokens |

use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use ms_common::{Error, Result};
use ms_media::transcode::Format;

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub music_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub master_key_file: PathBuf,
    pub users_file: PathBuf,
    pub trusted_proxies: Vec<IpAddr>,
    pub secure_cookies: bool,
    pub session_seconds: u64,
    pub scan_interval: Option<Duration>,
    pub max_tracks: usize,
    pub ffmpeg: PathBuf,
    pub max_transcodes: usize,
    pub max_cover_resizes: usize,
    pub tmp_dir: PathBuf,
    pub default_format: Format,
    pub transcode_bitrate: u32,
    pub cover_cache_bytes: u64,
    pub allow_password_auth: bool,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

fn number(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    match var(name) {
        None => Ok(default),
        Some(v) => {
            ms_common::buf::parse_u64(v.as_bytes(), max).filter(|&n| n >= min).ok_or_else(|| {
                Error::new(format!("{name} must be a whole number from {min} to {max}"))
            })
        }
    }
}

fn flag(name: &str, default: bool) -> Result<bool> {
    match var(name).as_deref() {
        None => Ok(default),
        Some("true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(_) => Err(Error::new(format!("{name} must be true or false"))),
    }
}

fn path(name: &str, default: &str) -> Result<PathBuf> {
    let p = PathBuf::from(var(name).unwrap_or_else(|| default.to_owned()));
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(Error::new(format!("{name} must be an absolute path")))
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let listen = var("MS_LISTEN")
            .unwrap_or_else(|| "127.0.0.1:8080".to_owned())
            .parse()
            .map_err(|_| Error::new("MS_LISTEN must be an address such as 0.0.0.0:8080"))?;
        let trusted_proxies = match var("MS_TRUSTED_PROXIES") {
            None => Vec::new(),
            Some(list) => list
                .split(',')
                .map(|ip| ip.trim().parse())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| Error::new("MS_TRUSTED_PROXIES must list IP addresses"))?,
        };
        let default_format = match var("MS_DEFAULT_TRANSCODE_FORMAT").as_deref() {
            None | Some("mp3") => Format::Mp3,
            Some("opus") => Format::Opus,
            Some(_) => return Err(Error::new("MS_DEFAULT_TRANSCODE_FORMAT must be mp3 or opus")),
        };
        let minutes = number("MS_SCAN_INTERVAL_MINUTES", 60, 0, 10_080)?;
        let as_usize = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        Ok(Self {
            listen,
            music_dir: path("MS_MUSIC_DIR", "/music")?,
            data_dir: path("MS_DATA_DIR", "/var/lib/music-server")?,
            cache_dir: path("MS_CACHE_DIR", "/var/cache/music-server")?,
            master_key_file: path("MS_MASTER_KEY_FILE", "/run/secrets/music_server_master_key")?,
            users_file: path("MS_USERS_FILE", "/run/secrets/music_server_users")?,
            trusted_proxies,
            secure_cookies: flag("MS_SECURE_COOKIES", true)?,
            session_seconds: number("MS_SESSION_HOURS", 12, 1, 720)? * 3600,
            scan_interval: (minutes > 0).then(|| Duration::from_mins(minutes)),
            max_tracks: as_usize(number("MS_MAX_TRACKS", 200_000, 1, 2_000_000)?),
            ffmpeg: path("MS_FFMPEG", "/usr/bin/ffmpeg")?,
            max_transcodes: as_usize(number("MS_MAX_TRANSCODES", 2, 0, 8)?),
            max_cover_resizes: as_usize(number("MS_MAX_COVER_RESIZES", 2, 0, 8)?),
            tmp_dir: path("MS_TMP_DIR", "/tmp")?,
            default_format,
            transcode_bitrate: u32::try_from(number("MS_TRANSCODE_BITRATE", 192, 32, 320)?)
                .unwrap_or(192),
            cover_cache_bytes: number("MS_COVER_CACHE_MB", 256, 0, 10_240)? * 1024 * 1024,
            allow_password_auth: flag("MS_ALLOW_PASSWORD_AUTH", true)?,
        })
    }
}
