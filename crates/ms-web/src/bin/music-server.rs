//! The music-server entry point.
//!
//! ```text
//! music-server [serve]      Run the server (the container's command)
//! music-server gen-key      Print a new 256-bit master key as 64 hex digits
//! music-server check-users  Validate MS_USERS_FILE and list its accounts
//!                           (names and admin flags; never passwords)
//! music-server scan-report  Scan MS_MUSIC_DIR once, read-only, and report
//!                           what was found, what failed, and the memory used
//! music-server healthcheck  Exit 0 when the local server answers /healthz
//! music-server version      Print the version
//! ```
//!
//! Configuration comes from the environment; see `ms_web::config`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use ms_common::{Error, Result};
use ms_crypto::MasterKey;
use ms_web::config::Config;

fn serve() -> Result<()> {
    // As PID 1 in a container an unhandled SIGTERM is ignored, so `podman
    // stop` would wait and then SIGKILL. The signals are blocked before any
    // thread starts, and one thread takes them: it saves the play counts
    // still in memory and exits.
    let signals = ms_sys::posix::TerminationSignals::new()?;
    signals.block()?;
    let config = Config::from_env()?;
    let listen = config.listen;
    let app = Arc::new(ms_web::build(config)?);
    let store = Arc::clone(&app.api.store);
    std::thread::Builder::new()
        .name("signals".to_owned())
        .stack_size(64 * 1024)
        .spawn(move || {
            if let Ok(sig) = signals.wait() {
                if let Err(e) = store.flush() {
                    eprintln!("music-server: cannot save play counts: {e}");
                }
                eprintln!("music-server: signal {sig}; stopping");
                std::process::exit(0);
            }
        })
        .map_err(|e| Error::new(format!("cannot start the signal thread: {e}")))?;
    let listener = TcpListener::bind(listen)
        .map_err(|e| Error::new(format!("cannot listen on {listen}: {e}")))?;
    eprintln!("music-server {} listening on {listen}", ms_common::VERSION);
    ms_http::serve(&listener, &app);
    Ok(())
}

fn gen_key() -> Result<()> {
    let key = MasterKey::generate_hex().map_err(|_| Error::new("randomness unavailable"))?;
    let mut out = io::stdout().lock();
    out.write_all(key.expose())?;
    out.write_all(b"\n")?;
    Ok(())
}

fn check_users() -> Result<()> {
    let config = Config::from_env()?;
    let users = ms_web::load_users(&config)?;
    for u in &users {
        println!("{}{}", u.name, if u.admin { " (admin)" } else { "" });
    }
    eprintln!("{} account(s); the users file is valid.", users.len());
    Ok(())
}

/// Peak resident memory of this process, from /proc.
fn peak_rss_kib() -> Option<u64> {
    let status = ms_common::fsio::read_text("/proc/self/status", 64 * 1024).ok()??;
    status
        .split('\n')
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
}

fn scan_report() -> Result<()> {
    let config = Config::from_env()?;
    let started = Instant::now();
    let progress = ms_library::scan::Progress::default();
    let result = ms_library::scan::scan(
        &config.music_dir,
        &HashMap::new(),
        config.max_tracks,
        true,
        &progress,
    )?;
    let scan_ms = started.elapsed().as_millis();
    let errors = result.errors.clone();
    let (error_count, truncated) = (result.error_count, result.truncated);
    let lib = ms_library::build::build(result.records, result.folder_covers, 0);
    println!("music directory: {}", config.music_dir.display());
    println!("tracks:  {}", lib.tracks.len());
    println!("albums:  {}", lib.albums.len());
    println!("artists: {}", lib.artists_sorted.len());
    println!("folders: {}", lib.dirs.len());
    println!("genres:  {}", lib.genres.len());
    println!("scan:    {scan_ms} ms");
    if let Some(kib) = peak_rss_kib() {
        println!("peak memory: {} MiB", kib / 1024);
    }
    if truncated {
        println!("note: stopped at MS_MAX_TRACKS");
    }
    println!("problems: {error_count}");
    for (path, message) in errors.iter().take(50) {
        println!("  {path}: {message}");
    }
    Ok(())
}

/// GET /healthz on the configured listener (loopback for a wildcard).
fn healthcheck() -> Result<()> {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;
    let config = Config::from_env()?;
    let mut addr = config.listen;
    if addr.ip().is_unspecified() {
        addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port());
    }
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
    let mut head = [0_u8; 12];
    stream.read_exact(&mut head)?;
    if &head == b"HTTP/1.1 200" { Ok(()) } else { Err(Error::new("unhealthy")) }
}

fn main() -> ExitCode {
    let command = std::env::args().nth(1).unwrap_or_else(|| "serve".to_owned());
    let result = match command.as_str() {
        "serve" => serve(),
        "gen-key" => gen_key(),
        "check-users" => check_users(),
        "scan-report" => scan_report(),
        "healthcheck" => healthcheck(),
        "version" | "--version" => {
            println!("music-server {}", ms_common::VERSION);
            Ok(())
        }
        _ => Err(Error::new(
            "usage: music-server [serve|gen-key|check-users|scan-report|healthcheck|version]",
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("music-server: {e}");
            ExitCode::FAILURE
        }
    }
}
