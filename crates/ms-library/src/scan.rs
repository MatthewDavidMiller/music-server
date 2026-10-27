//! Walking the music directory.
//!
//! The walk never follows a symbolic link (directory entries are typed
//! without following, and audio files are opened with `O_NOFOLLOW`), skips
//! hidden entries, stops descending at [`MAX_DEPTH`], and stops collecting
//! at the configured track ceiling. A file whose size and modification time
//! match the previous scan keeps its previous record without being opened,
//! so a rescan of an unchanged library is a walk of `stat` calls.

use std::collections::HashMap;
use std::fs;
use std::hash::BuildHasher;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ms_common::{Error, Result, fsio};
use ms_tags::FileSource;

use crate::model::TrackRecord;

pub const MAX_DEPTH: usize = 32;
pub const MAX_REPORTED_ERRORS: usize = 200;
/// File extensions scanned as audio.
pub const AUDIO_EXTENSIONS: &[&str] =
    &["mp3", "flac", "m4a", "m4b", "mp4", "alac", "ogg", "oga", "opus", "wav"];
/// Folder images, most preferred first.
const COVER_NAMES: &[&str] = &["cover", "folder", "front", "album", "albumart"];
const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png"];

/// Counters a scan updates as it goes, for the status page.
#[derive(Debug, Default)]
pub struct Progress {
    pub files_seen: AtomicU64,
    pub probed: AtomicU64,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub records: Vec<TrackRecord>,
    pub folder_covers: Vec<(String, String)>,
    /// (relative path, message), at most [`MAX_REPORTED_ERRORS`].
    pub errors: Vec<(String, String)>,
    pub error_count: u64,
    pub truncated: bool,
}

impl ScanResult {
    fn error(&mut self, path: &str, message: impl Into<String>) {
        self.error_count += 1;
        if self.errors.len() < MAX_REPORTED_ERRORS {
            self.errors.push((path.to_owned(), message.into()));
        }
    }
}

fn extension(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
}

fn cover_rank(name: &str) -> Option<usize> {
    let (stem, ext) = name.rsplit_once('.')?;
    let e = COVER_EXTENSIONS.iter().position(|x| ext.eq_ignore_ascii_case(x))?;
    let n = COVER_NAMES.iter().position(|x| stem.eq_ignore_ascii_case(x))?;
    Some(n * COVER_EXTENSIONS.len() + e)
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() { name.to_owned() } else { format!("{rel}/{name}") }
}

/// Reads one file's metadata into a record.
pub fn probe_file(
    abs: &Path,
    path: &str,
    size: u64,
    mtime_ns: i64,
    created: i64,
) -> Result<TrackRecord> {
    let file = fsio::open_nofollow(abs)?;
    let mut src = FileSource::new(file)?;
    let p = ms_tags::probe(&mut src)?;
    let container = p.container.ok_or_else(|| Error::new("unrecognised container"))?;
    Ok(TrackRecord {
        path: path.to_owned(),
        size,
        mtime_ns,
        created,
        container,
        codec: p.audio.codec,
        title: p.tags.title,
        artist: p.tags.artist,
        album: p.tags.album,
        album_artist: p.tags.album_artist,
        track_no: p.tags.track.unwrap_or(0),
        disc_no: p.tags.disc.unwrap_or(0),
        year: p.tags.year.unwrap_or(0),
        genres: p.tags.genres,
        compilation: p.tags.compilation,
        duration_ms: p.audio.duration_ms,
        bitrate_kbps: p.audio.bitrate_kbps,
        sample_rate: p.audio.sample_rate,
        channels: p.audio.channels,
        bit_depth: p.audio.bit_depth,
        picture: p.picture,
    })
}

/// Scans `root`. `previous` maps relative paths to the last scan's records;
/// `first_scan` dates new files by their modification time rather than
/// now, so the first "newest albums" list is meaningful.
pub fn scan<S: BuildHasher>(
    root: &Path,
    previous: &HashMap<String, TrackRecord, S>,
    max_tracks: usize,
    first_scan: bool,
    progress: &Progress,
) -> Result<ScanResult> {
    let meta =
        fs::symlink_metadata(root).map_err(|e| Error::new(format!("music directory: {e}")))?;
    if !meta.is_dir() {
        return Err(Error::new("the music directory is not a directory"));
    }
    let now = ms_common::timefmt::unix_seconds();
    let mut out = ScanResult::default();
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((abs, rel, depth)) = stack.pop() {
        let entries = match fs::read_dir(&abs) {
            Ok(e) => e,
            Err(e) => {
                out.error(&rel, format!("cannot list directory: {e}"));
                continue;
            }
        };
        let mut best_cover: Option<(usize, String)> = None;
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let os_name = entry.file_name();
            let Some(name) = os_name.to_str() else {
                out.error(&rel, "a file name is not valid UTF-8 and was skipped");
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if depth + 1 < MAX_DEPTH {
                    stack.push((entry.path(), join(&rel, name), depth + 1));
                } else {
                    out.error(&join(&rel, name), "nested too deeply; not scanned");
                }
            } else if kind.is_file() {
                if let Some(rank) = cover_rank(name)
                    && best_cover.as_ref().is_none_or(|(r, _)| rank < *r)
                {
                    best_cover = Some((rank, join(&rel, name)));
                }
                if extension(name).is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.as_str())) {
                    files.push((name.to_owned(), entry.path()));
                }
            }
        }
        if let Some((_, cover)) = best_cover {
            out.folder_covers.push((rel.clone(), cover));
        }
        for (name, path) in files {
            if out.records.len() >= max_tracks {
                out.truncated = true;
                break;
            }
            progress.files_seen.fetch_add(1, Ordering::Relaxed);
            let rel_path = join(&rel, &name);
            let Ok(m) = fs::symlink_metadata(&path) else { continue };
            if !m.is_file() {
                continue;
            }
            let mtime_ns = m.mtime().saturating_mul(1_000_000_000).saturating_add(m.mtime_nsec());
            let prev = previous.get(&rel_path);
            if let Some(p) = prev
                && p.size == m.len()
                && p.mtime_ns == mtime_ns
            {
                out.records.push(p.clone());
                continue;
            }
            let created = prev.map_or(if first_scan { m.mtime() } else { now }, |p| p.created);
            progress.probed.fetch_add(1, Ordering::Relaxed);
            match probe_file(&path, &rel_path, m.len(), mtime_ns, created) {
                Ok(r) => out.records.push(r),
                Err(e) => out.error(&rel_path, e.message()),
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests_support {
    use ms_tags::testgen::{Id3Frame, id3v2, mpeg_frames};

    pub(crate) fn mp3(title: &str, album: &str) -> Vec<u8> {
        let mut f = id3v2(
            3,
            &[
                Id3Frame::text("TIT2", title),
                Id3Frame::text("TALB", album),
                Id3Frame::text("TPE1", "Artist"),
            ],
            false,
        );
        f.extend(mpeg_frames(20, false));
        f
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::mp3;
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ms-scan-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walks_reuses_and_refuses_links() {
        let root = scratch("walk");
        fs::create_dir_all(root.join("A/Album")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join("A/Album/01.mp3"), mp3("One", "Album")).unwrap();
        fs::write(root.join("A/Album/02.MP3"), mp3("Two", "Album")).unwrap();
        fs::write(root.join("A/Album/notes.txt"), b"x").unwrap();
        fs::write(root.join("A/Album/Folder.JPG"), b"x").unwrap();
        fs::write(root.join("A/Album/cover.png"), b"x").unwrap();
        fs::write(root.join("A/Album/bad.mp3"), b"not audio").unwrap();
        fs::write(root.join(".hidden/x.mp3"), mp3("Hidden", "H")).unwrap();
        let outside = scratch("outside");
        fs::write(outside.join("secret.mp3"), mp3("Secret", "S")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.mp3"), root.join("A/link.mp3")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();

        let progress = Progress::default();
        let first = scan(&root, &HashMap::new(), 100, true, &progress).unwrap();
        let mut titles: Vec<_> = first.records.iter().map(|r| r.title.clone().unwrap()).collect();
        titles.sort();
        assert_eq!(titles, ["One", "Two"]);
        assert_eq!(first.errors.len(), 1);
        assert_eq!(first.errors[0].0, "A/Album/bad.mp3");
        assert_eq!(first.folder_covers, [("A/Album".to_owned(), "A/Album/cover.png".to_owned())]);
        assert_eq!(progress.probed.load(Ordering::Relaxed), 3);

        // Unchanged files are not opened again.
        let previous: HashMap<_, _> =
            first.records.iter().map(|r| (r.path.clone(), r.clone())).collect();
        let again = Progress::default();
        let second = scan(&root, &previous, 100, false, &again).unwrap();
        assert_eq!(second.records.len(), 2);
        assert_eq!(again.probed.load(Ordering::Relaxed), 1); // only bad.mp3
        // The ceiling stops collection.
        let capped = scan(&root, &previous, 1, false, &Progress::default()).unwrap();
        assert!(capped.truncated && capped.records.len() == 1);
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn root_must_be_a_directory() {
        assert!(
            scan(Path::new("/nonexistent-ms"), &HashMap::new(), 10, true, &Progress::default())
                .is_err()
        );
    }
}
