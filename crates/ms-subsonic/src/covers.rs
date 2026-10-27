//! The resized-cover cache: `<cache>/covers/<2 hex>/<key>.jpg`.
//!
//! A key names the source image (which entity, the source file's size and
//! modification time) and the size bucket, so a changed file misses rather
//! than serving a stale picture. The cache is capped; past the cap the
//! least recently used files go until it is under 90% of it. Hits refresh a
//! file's modification time, which is what "recently used" means here.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use ms_common::{Result, fsio};
use ms_media::cover::MAX_OUTPUT;
use ms_store::lock;

#[derive(Debug)]
pub struct CoverCache {
    dir: PathBuf,
    max_bytes: u64,
    total: Mutex<u64>,
}

fn walk(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut out = Vec::new();
    let Ok(shards) = fs::read_dir(dir) else { return out };
    for shard in shards.flatten() {
        let Ok(files) = fs::read_dir(shard.path()) else { continue };
        for f in files.flatten() {
            if let Ok(m) = f.metadata()
                && m.is_file()
            {
                out.push((f.path(), m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
            }
        }
    }
    out
}

impl CoverCache {
    pub fn new(dir: &Path, max_bytes: u64) -> Result<Self> {
        fsio::mkdir_all(dir, 0o700)?;
        let total = walk(dir).iter().map(|(_, n, _)| n).sum();
        Ok(Self { dir: dir.to_path_buf(), max_bytes, total: Mutex::new(total) })
    }

    /// The cache key for a source and size bucket.
    pub fn key(source: &str, stamp: (u64, i64), bucket: u32) -> String {
        let input = format!("{source}\0{}\0{}\0{bucket}", stamp.0, stamp.1);
        ms_crypto::sha256(input.as_bytes()).map(|d| ms_crypto::hex(&d)).unwrap_or_default()
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        (key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| self.dir.join(&key[..2]).join(format!("{key}.jpg")))
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.path(key)?;
        let data = fsio::read_bounded(&path, MAX_OUTPUT).ok()??;
        if let Ok(f) = File::options().write(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(data)
    }

    pub fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        if self.max_bytes == 0 {
            return Ok(());
        }
        let Some(path) = self.path(key) else { return Ok(()) };
        if let Some(parent) = path.parent() {
            fsio::mkdir_all(parent, 0o700)?;
        }
        fsio::atomic_replace(&path, data, MAX_OUTPUT)?;
        let mut total = lock(&self.total);
        *total += data.len() as u64;
        if *total > self.max_bytes {
            let mut files = walk(&self.dir);
            files.sort_by_key(|(_, _, t)| *t);
            let mut sum: u64 = files.iter().map(|(_, n, _)| n).sum();
            let target = self.max_bytes / 10 * 9;
            for (p, n, _) in files {
                if sum <= target {
                    break;
                }
                if fs::remove_file(&p).is_ok() {
                    sum -= n;
                }
            }
            *total = sum;
        }
        Ok(())
    }

    pub fn total_bytes(&self) -> u64 {
        *lock(&self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_prunes() {
        let dir = std::env::temp_dir().join(format!("ms-covers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = CoverCache::new(&dir, 2500).unwrap();
        let keys: Vec<String> = (0..4).map(|i| CoverCache::key("al-x", (1, 2), i)).collect();
        assert_ne!(keys[0], keys[1]);
        assert_eq!(cache.get(&keys[0]), None);
        cache.put(&keys[0], &[1; 1000]).unwrap();
        assert_eq!(cache.get(&keys[0]).unwrap().len(), 1000);
        cache.put(&keys[1], &[2; 1000]).unwrap();
        cache.put(&keys[2], &[3; 1000]).unwrap();
        assert!(cache.total_bytes() <= 2250);
        assert_eq!(cache.get("../../etc/passwd"), None);
        // Reopening counts what is there.
        assert_eq!(CoverCache::new(&dir, 2500).unwrap().total_bytes(), cache.total_bytes());
        fs::remove_dir_all(&dir).unwrap();
    }
}
