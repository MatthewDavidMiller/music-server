//! The one place this server runs another program: ffmpeg, for transcoded
//! streams and for resized cover art.
//!
//! Every invocation is a fixed argument vector built here, never a shell
//! line, with:
//!
//! - `-protocol_whitelist file` and a `file:` input, so a file name can be
//!   neither an option nor a URL;
//! - a cleared environment, `/dev/null` for stdin, and `/` as the working
//!   directory;
//! - its own process group, killed with SIGKILL and reaped when the
//!   [`Supervised`] handle drops (a client that disconnects mid-stream makes
//!   the write fail, which drops the handle);
//! - a wall-clock deadline enforced by a watchdog thread;
//! - a concurrency cap ([`Slots`]), so a burst of requests cannot start
//!   more encoders than the container's memory allows.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod cover;
pub mod transcode;

use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ms_common::{Error, Result};
use ms_sys::posix::{SIGKILL, send_signal};

const HELPER_STACK: usize = 64 * 1024;
/// How much of a child's stderr is kept for the log.
const STDERR_KEEP: usize = 4096;

/// A fixed number of concurrent jobs of one kind.
#[derive(Clone, Debug)]
pub struct Slots {
    inner: Arc<SlotsInner>,
}

#[derive(Debug)]
struct SlotsInner {
    max: usize,
    active: AtomicUsize,
}

/// One held slot, released on drop.
#[derive(Debug)]
pub struct Permit {
    inner: Arc<SlotsInner>,
}

impl Slots {
    pub fn new(max: usize) -> Self {
        Self { inner: Arc::new(SlotsInner { max, active: AtomicUsize::new(0) }) }
    }

    pub fn try_acquire(&self) -> Option<Permit> {
        let mut current = self.inner.active.load(Ordering::Acquire);
        loop {
            if current >= self.inner.max {
                return None;
            }
            match self.inner.active.compare_exchange(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Permit { inner: Arc::clone(&self.inner) }),
                Err(seen) => current = seen,
            }
        }
    }

    pub fn active(&self) -> usize {
        self.inner.active.load(Ordering::Acquire)
    }

    pub fn max(&self) -> usize {
        self.inner.max
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.inner.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A running ffmpeg: killed (its whole process group) and reaped on drop.
#[derive(Debug)]
pub struct Supervised {
    child: Child,
    pgid: i32,
    stdout: Option<ChildStdout>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    watchdog: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}

impl Supervised {
    /// Starts `ffmpeg` with `args`, to be killed after `deadline`.
    pub fn spawn(ffmpeg: &Path, args: &[OsString], deadline: Duration) -> Result<Self> {
        let mut child = Command::new(ffmpeg)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| Error::new(format!("cannot start ffmpeg: {e}")))?;
        let pgid = i32::try_from(child.id()).map_err(|_| Error::new("process id out of range"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take().and_then(|mut err| {
            thread::Builder::new()
                .name("ffmpeg-stderr".to_owned())
                .stack_size(HELPER_STACK)
                .spawn(move || {
                    let mut kept = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        match err.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => {
                                let room = STDERR_KEEP.saturating_sub(kept.len());
                                kept.extend_from_slice(&chunk[..n.min(room)]);
                            }
                            Err(e) if e.kind() == ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                    kept
                })
                .ok()
        });
        let (done, wait) = mpsc::channel::<()>();
        let watchdog = thread::Builder::new()
            .name("ffmpeg-watchdog".to_owned())
            .stack_size(HELPER_STACK)
            .spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(deadline) {
                    let _ = send_signal(-pgid, SIGKILL);
                }
            })
            .ok()
            .map(|handle| (done, handle));
        let s = Self { child, pgid, stdout, stderr, watchdog };
        if s.watchdog.is_none() {
            // Without a deadline the process must not run at all.
            return Err(Error::new("cannot start the ffmpeg watchdog"));
        }
        Ok(s)
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn stdout(&mut self) -> Option<&mut ChildStdout> {
        self.stdout.as_mut()
    }

    /// Waits for the process to exit and returns its status and the start
    /// of its stderr.
    pub fn finish(mut self) -> Result<(ExitStatus, String)> {
        self.stdout = None;
        let status = self.child.wait()?;
        self.stop_watchdog();
        let err = self.stderr.take().and_then(|h| h.join().ok()).unwrap_or_default();
        Ok((status, String::from_utf8_lossy(&err).trim().to_owned()))
    }

    fn stop_watchdog(&mut self) {
        if let Some((done, handle)) = self.watchdog.take() {
            drop(done);
            let _ = handle.join();
        }
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        self.stdout = None;
        if let Ok(None) = self.child.try_wait() {
            let _ = send_signal(-self.pgid, SIGKILL);
        }
        // The watchdog may only signal the group while the child is not yet
        // reaped, or the id could belong to another process by then.
        self.stop_watchdog();
        let _ = self.child.wait();
        if let Some(h) = self.stderr.take() {
            let _ = h.join();
        }
    }
}

/// Reads a child's stdout to the end, at most `max` bytes.
pub(crate) fn read_all_bounded(out: &mut ChildStdout, max: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        match out.read(&mut chunk) {
            Ok(0) => return Ok(data),
            Ok(n) => {
                if data.len() + n > max {
                    return Err(Error::new("ffmpeg output exceeds its size limit"));
                }
                data.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// The input argument for a file: `file:` so the name is never parsed as a
/// protocol or option.
pub(crate) fn input_arg(path: &Path) -> OsString {
    let mut a = OsString::from("file:");
    a.push(path.as_os_str());
    a
}

/// Which of the encoders this server uses the installed ffmpeg has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Encoders {
    pub mp3: bool,
    pub opus: bool,
    pub mjpeg: bool,
}

impl Encoders {
    pub fn probe(ffmpeg: &Path) -> Result<Self> {
        let args: Vec<OsString> =
            ["-hide_banner", "-encoders"].iter().map(OsString::from).collect();
        let mut child = Supervised::spawn(ffmpeg, &args, Duration::from_secs(15))?;
        let out = match child.stdout() {
            Some(o) => read_all_bounded(o, 1024 * 1024)?,
            None => Vec::new(),
        };
        let (status, err) = child.finish()?;
        if !status.success() {
            return Err(Error::new(format!("ffmpeg -encoders failed: {err}")));
        }
        let text = String::from_utf8_lossy(&out);
        let has = |name: &str| text.lines().any(|l| l.split_whitespace().nth(1) == Some(name));
        Ok(Self { mp3: has("libmp3lame"), opus: has("libopus"), mjpeg: has("mjpeg") })
    }
}

/// The ffmpeg configuration shared by every job.
#[derive(Clone, Debug)]
pub struct Media {
    pub ffmpeg: PathBuf,
    pub encoders: Encoders,
    pub transcodes: Slots,
    pub resizes: Slots,
    /// Where embedded cover art is staged for a resize (a tmpfs).
    pub scratch: PathBuf,
}

impl Media {
    /// Whether a stream may be transcoded to `format` at all.
    pub fn can_transcode(&self, format: transcode::Format) -> bool {
        self.transcodes.max() > 0
            && match format {
                transcode::Format::Mp3 => self.encoders.mp3,
                transcode::Format::Opus => self.encoders.opus,
            }
    }

    pub fn can_resize(&self) -> bool {
        self.resizes.max() > 0 && self.encoders.mjpeg
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;
    use std::process::Command;

    /// ffmpeg on PATH, or `None` (a skip) unless MS_REQUIRE_FFMPEG is set.
    pub(crate) fn ffmpeg() -> Option<PathBuf> {
        for dir in ["/usr/bin", "/usr/local/bin", "/bin"] {
            let p = PathBuf::from(dir).join("ffmpeg");
            if p.exists() {
                return Some(p);
            }
        }
        assert!(
            std::env::var_os("MS_REQUIRE_FFMPEG").is_none(),
            "MS_REQUIRE_FFMPEG is set but ffmpeg is missing"
        );
        eprintln!("skipping: ffmpeg is not installed");
        None
    }

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ms-media-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn tone(ffmpeg: &std::path::Path, out: &std::path::Path, seconds: u32) {
        let status = Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={seconds}"))
            .args(["-c:a", "flac"])
            .arg(out)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_cap_and_release() {
        let s = Slots::new(2);
        let a = s.try_acquire().unwrap();
        let _b = s.try_acquire().unwrap();
        assert!(s.try_acquire().is_none());
        drop(a);
        assert_eq!(s.active(), 1);
        assert!(s.try_acquire().is_some());
        assert!(Slots::new(0).try_acquire().is_none());
    }

    #[test]
    fn encoders_are_detected() {
        let Some(ff) = testutil::ffmpeg() else { return };
        let e = Encoders::probe(&ff).unwrap();
        assert!(e.mp3 && e.opus && e.mjpeg, "{e:?}");
        assert!(Encoders::probe(Path::new("/nonexistent/ffmpeg")).is_err());
    }

    #[test]
    fn the_watchdog_kills_a_process_past_its_deadline() {
        let Some(ff) = testutil::ffmpeg() else { return };
        // An endless input: only the deadline ends this.
        let args: Vec<OsString> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "anullsrc",
            "-f",
            "null",
            "-",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let started = std::time::Instant::now();
        let child = Supervised::spawn(&ff, &args, Duration::from_millis(500)).unwrap();
        let (status, _) = child.finish().unwrap();
        assert!(!status.success());
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
