//! The data directory: the sealed account roster and each user's sealed
//! playlists, stars, and play counts.
//!
//! ```text
//! <data>/users.sealed            the roster (label "users")
//! <data>/user/<hash>.sealed      one user's data (label "user/<name>"),
//!                                <hash> = the first 16 bytes of
//!                                SHA-256(name), in hex
//! ```
//!
//! Playlist and star changes are written through before the request that
//! made them returns. Play counts change on every scrobble, so they are
//! marked dirty and written by the flush thread every few seconds; a crash
//! loses at most that many seconds of counts.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod jsonw;
pub mod sealed;
pub mod userdata;
pub mod users;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use ms_common::{Error, Result, fsio};
use ms_crypto::MasterKey;

pub use sealed::Sealer;
pub use userdata::{Play, Playlist, Star, UserData};
pub use users::{RosterUser, SyncReport, UserEntry};

const ROSTER_LABEL: &str = "users";
const MAX_ROSTER_FILE: usize = 1024 * 1024;
const MAX_USER_FILE: usize = 64 * 1024 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const THREAD_STACK: usize = 256 * 1024;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    sealer: Sealer,
    users: Mutex<HashMap<String, Arc<Mutex<UserData>>>>,
    dirty: Mutex<HashSet<String>>,
}

impl Store {
    pub fn open(dir: &Path, master: &MasterKey) -> Result<Self> {
        fsio::mkdir_all(dir, 0o700)?;
        fsio::mkdir_all(dir.join("user"), 0o700)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            sealer: Sealer::new(master)?,
            users: Mutex::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
        })
    }

    pub fn roster(&self) -> Result<Vec<users::RosterUser>> {
        match fsio::read_bounded(self.dir.join("users.sealed"), MAX_ROSTER_FILE)? {
            None => Ok(Vec::new()),
            Some(raw) => users::decode_roster(self.sealer.open(ROSTER_LABEL, &raw)?.expose()),
        }
    }

    pub fn save_roster(&self, roster: &[users::RosterUser]) -> Result<()> {
        let sealed = self.sealer.seal(ROSTER_LABEL, users::encode_roster(roster).as_bytes())?;
        fsio::atomic_replace(self.dir.join("users.sealed"), &sealed, MAX_ROSTER_FILE)
    }

    /// Syncs the stored roster to the users file and saves it.
    pub fn sync_roster(&self, entries: &[UserEntry]) -> Result<(Vec<RosterUser>, SyncReport)> {
        let mut roster = self.roster()?;
        let report = users::sync(&mut roster, entries, ms_common::timefmt::unix_seconds());
        if !report.is_empty() {
            self.save_roster(&roster)?;
        }
        Ok((roster, report))
    }

    fn user_path(&self, name: &str) -> PathBuf {
        let digest = ms_crypto::sha256(name.as_bytes()).unwrap_or([0; 32]);
        self.dir.join("user").join(format!("{}.sealed", ms_crypto::hex(&digest[..16])))
    }

    fn label(name: &str) -> String {
        format!("user/{name}")
    }

    /// A user's data, loaded on first use.
    pub fn user(&self, name: &str) -> Result<Arc<Mutex<UserData>>> {
        if let Some(d) = lock(&self.users).get(name) {
            return Ok(Arc::clone(d));
        }
        let data = match fsio::read_bounded(self.user_path(name), MAX_USER_FILE)? {
            None => UserData::default(),
            Some(raw) => UserData::decode(self.sealer.open(&Self::label(name), &raw)?.expose())?,
        };
        let mut users = lock(&self.users);
        Ok(Arc::clone(users.entry(name.to_owned()).or_insert_with(|| Arc::new(Mutex::new(data)))))
    }

    fn write(&self, name: &str, data: &UserData) -> Result<()> {
        let sealed = self.sealer.seal(&Self::label(name), data.encode().as_bytes())?;
        fsio::atomic_replace(self.user_path(name), &sealed, MAX_USER_FILE)
    }

    /// Changes a user's data and writes it before returning.
    pub fn update<R>(
        &self,
        name: &str,
        change: impl FnOnce(&mut UserData) -> Result<R>,
    ) -> Result<R> {
        let cell = self.user(name)?;
        let mut data = lock(&cell);
        let mut copy = data.clone();
        let result = change(&mut copy)?;
        self.write(name, &copy)?;
        *data = copy;
        lock(&self.dirty).remove(name);
        Ok(result)
    }

    /// Changes a user's data; the flush thread writes it soon after.
    pub fn update_later(&self, name: &str, change: impl FnOnce(&mut UserData)) -> Result<()> {
        let cell = self.user(name)?;
        change(&mut lock(&cell));
        lock(&self.dirty).insert(name.to_owned());
        Ok(())
    }

    /// Writes every user changed by [`Store::update_later`].
    pub fn flush(&self) -> Result<()> {
        let names: Vec<String> = lock(&self.dirty).drain().collect();
        let mut first_error = None;
        for name in names {
            let cell = self.user(&name)?;
            let snapshot = lock(&cell).clone();
            if let Err(e) = self.write(&name, &snapshot) {
                lock(&self.dirty).insert(name);
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn start_flusher(self: &Arc<Self>) -> Result<()> {
        let me = Arc::clone(self);
        thread::Builder::new()
            .name("flush".to_owned())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                loop {
                    thread::sleep(FLUSH_INTERVAL);
                    if let Err(e) = me.flush() {
                        eprintln!("music-server: cannot save play counts: {e}");
                    }
                }
            })
            .map(|_| ())
            .map_err(|e| Error::new(format!("cannot start the flush thread: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ms-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn key() -> MasterKey {
        MasterKey::from_hex(&[b'4'; 64]).unwrap()
    }

    #[test]
    fn user_data_persists_sealed() {
        let dir = scratch("persist");
        let store = Store::open(&dir, &key()).unwrap();
        store
            .update("alice", |d| {
                d.playlists.push(Playlist {
                    id: "pl-1".into(),
                    name: "Secret Mix".into(),
                    ..Playlist::default()
                });
                Ok(())
            })
            .unwrap();
        store.update_later("alice", |d| d.record_play("tr-1", 5)).unwrap();
        store.flush().unwrap();
        let file = std::fs::read(store.user_path("alice")).unwrap();
        assert!(Sealer::is_sealed(&file));
        assert!(!file.windows(6).any(|w| w == b"Secret"));
        let reopened = Store::open(&dir, &key()).unwrap();
        let alice = reopened.user("alice").unwrap();
        assert_eq!(lock(&alice).playlists[0].name, "Secret Mix");
        assert_eq!(lock(&alice).plays["tr-1"].count, 1);
        // Another user's file is not interchangeable.
        std::fs::copy(store.user_path("alice"), reopened.user_path("bob")).unwrap();
        assert!(Store::open(&dir, &key()).unwrap().user("bob").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_updates_change_nothing() {
        let dir = scratch("failed");
        let store = Store::open(&dir, &key()).unwrap();
        let r: Result<()> = store.update("a", |d| {
            d.star("tr-1", 1)?;
            Err(Error::new("nope"))
        });
        assert!(r.is_err());
        assert!(lock(&store.user("a").unwrap()).starred.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn roster_round_trips() {
        let dir = scratch("roster");
        let store = Store::open(&dir, &key()).unwrap();
        assert!(store.roster().unwrap().is_empty());
        let entries = users::parse_users_file(
            br#"{"version":1,"users":[{"name":"a","password":"12345678","admin":true}]}"#,
        )
        .unwrap();
        let (roster, report) = store.sync_roster(&entries).unwrap();
        assert_eq!(report.added, ["a"]);
        assert_eq!(store.roster().unwrap(), roster);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
