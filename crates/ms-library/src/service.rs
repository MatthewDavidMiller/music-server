//! The running library: the current snapshot, the scan thread, and its
//! status.
//!
//! At startup the cached index is loaded and served immediately; a scan
//! then runs in the background. Scans run one at a time, on an interval
//! and on request; a request made during a scan is coalesced into one
//! follow-up scan. Each finished scan swaps in a new snapshot and rewrites
//! the cache.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use ms_common::{Error, Result, timefmt};

use crate::build::build;
use crate::cache::{self, Cached};
use crate::model::Library;
use crate::scan::{Progress, scan};

const THREAD_STACK: usize = 512 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Debug)]
pub struct LibraryConfig {
    pub music_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub max_tracks: usize,
    /// `None` scans only at startup and on request.
    pub interval: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
pub struct ScanStatus {
    pub scanning: bool,
    pub scans_completed: u64,
    pub last_started_ms: u64,
    pub last_finished_ms: u64,
    pub last_duration_ms: u64,
    pub files_seen: u64,
    pub files_probed: u64,
    pub error_count: u64,
    /// (relative path, message), the first few of the last scan.
    pub errors: Vec<(String, String)>,
    pub truncated: bool,
    /// Why the last scan could not run at all.
    pub failure: Option<String>,
}

#[derive(Debug)]
pub struct LibraryService {
    config: LibraryConfig,
    current: RwLock<Arc<Library>>,
    status: Mutex<ScanStatus>,
    progress: Progress,
    requested: Mutex<bool>,
    wake: Condvar,
}

impl LibraryService {
    /// Loads the cached index, if any. No scan has run yet.
    pub fn new(config: LibraryConfig) -> Self {
        let cached = cache::load(&config.cache_dir.join("library.idx")).unwrap_or_default();
        let library = build(cached.records, cached.folder_covers, 0);
        Self {
            config,
            current: RwLock::new(Arc::new(library)),
            status: Mutex::new(ScanStatus::default()),
            progress: Progress::default(),
            requested: Mutex::new(false),
            wake: Condvar::new(),
        }
    }

    pub fn config(&self) -> &LibraryConfig {
        &self.config
    }

    /// The current snapshot.
    pub fn library(&self) -> Arc<Library> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn status(&self) -> ScanStatus {
        let mut s = lock(&self.status).clone();
        if s.scanning {
            s.files_seen = self.progress.files_seen.load(Ordering::Relaxed);
            s.files_probed = self.progress.probed.load(Ordering::Relaxed);
        }
        s
    }

    /// Asks the scan thread for a scan as soon as it is free.
    pub fn request_scan(&self) {
        *lock(&self.requested) = true;
        self.wake.notify_all();
    }

    /// Runs one scan on this thread and installs its result.
    pub fn scan_now(&self) -> Result<()> {
        let started = Instant::now();
        {
            let mut s = lock(&self.status);
            s.scanning = true;
            s.last_started_ms = u64::try_from(timefmt::unix_ms()).unwrap_or(0);
        }
        self.progress.files_seen.store(0, Ordering::Relaxed);
        self.progress.probed.store(0, Ordering::Relaxed);
        let old = self.library();
        let previous: HashMap<String, _> =
            old.tracks.iter().map(|t| (t.rec.path.clone(), t.rec.clone())).collect();
        let first_scan = previous.is_empty();
        let result = scan(
            &self.config.music_dir,
            &previous,
            self.config.max_tracks,
            first_scan,
            &self.progress,
        );
        drop(previous);
        drop(old);
        let mut status = lock(&self.status);
        status.scanning = false;
        status.files_seen = self.progress.files_seen.load(Ordering::Relaxed);
        status.files_probed = self.progress.probed.load(Ordering::Relaxed);
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                status.failure = Some(e.message().to_owned());
                return Err(e);
            }
        };
        let finished = u64::try_from(timefmt::unix_ms()).unwrap_or(0);
        status.failure = None;
        status.scans_completed += 1;
        status.last_finished_ms = finished;
        status.last_duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        status.error_count = result.error_count;
        status.errors = result.errors;
        status.truncated = result.truncated;
        drop(status);
        let cached = Cached { records: result.records, folder_covers: result.folder_covers };
        if let Err(e) = cache::save(&self.config.cache_dir.join("library.idx"), &cached) {
            eprintln!("music-server: cannot write the library index cache: {e}");
        }
        let library = build(cached.records, cached.folder_covers, finished);
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(library);
        Ok(())
    }

    /// Starts the scan thread, which scans at once and then on the interval
    /// or on request.
    pub fn start(self: &Arc<Self>) -> Result<()> {
        let me = Arc::clone(self);
        thread::Builder::new()
            .name("scan".to_owned())
            .stack_size(THREAD_STACK)
            .spawn(move || me.run())
            .map(|_| ())
            .map_err(|e| Error::new(format!("cannot start the scan thread: {e}")))
    }

    fn run(&self) {
        loop {
            match self.scan_now() {
                Ok(()) => {
                    let s = self.status();
                    eprintln!(
                        "music-server: scan finished: {} tracks, {} read, {} problem(s), {} ms",
                        self.library().tracks.len(),
                        s.files_probed,
                        s.error_count,
                        s.last_duration_ms
                    );
                }
                Err(e) => eprintln!("music-server: scan failed: {e}"),
            }
            let mut requested = lock(&self.requested);
            let deadline = self.config.interval.map(|i| Instant::now() + i);
            while !*requested {
                match deadline {
                    None => {
                        requested =
                            self.wake.wait(requested).unwrap_or_else(PoisonError::into_inner);
                    }
                    Some(d) => {
                        let now = Instant::now();
                        if now >= d {
                            break;
                        }
                        requested = self
                            .wake
                            .wait_timeout(requested, d - now)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0;
                    }
                }
            }
            *requested = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scans_cache_and_restart() {
        let dir = std::env::temp_dir().join(format!("ms-service-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("music/A")).unwrap();
        fs::create_dir_all(dir.join("cache")).unwrap();
        fs::write(dir.join("music/A/1.mp3"), crate::scan::tests_support::mp3("One", "A")).unwrap();
        let config = LibraryConfig {
            music_dir: dir.join("music"),
            cache_dir: dir.join("cache"),
            max_tracks: 100,
            interval: None,
        };
        let svc = LibraryService::new(config.clone());
        assert!(svc.library().tracks.is_empty());
        svc.scan_now().unwrap();
        assert_eq!(svc.library().tracks.len(), 1);
        assert_eq!(svc.status().scans_completed, 1);
        // A restart serves the cached index before any scan.
        let again = LibraryService::new(config);
        assert_eq!(again.library().tracks.len(), 1);
        assert_eq!(again.library().tracks[0].id, svc.library().tracks[0].id);
        // A missing music directory is reported, and the snapshot kept.
        fs::remove_dir_all(dir.join("music")).unwrap();
        assert!(again.scan_now().is_err());
        assert!(again.status().failure.is_some());
        assert_eq!(again.library().tracks.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
