//! The library's records: tracks as scanned, and the albums, artists,
//! directories, and genres derived from them.
//!
//! A [`Library`] is immutable once built. A rescan builds a new one and the
//! service swaps it in, so a request always sees one consistent snapshot.

use std::collections::HashMap;

use ms_tags::{Codec, Container, PictureRef};

use crate::id::{ID_BYTES, Id, Kind};

/// One audio file, as the scanner found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackRecord {
    /// Relative to the library root, `/`-separated.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// When this path was first seen (Unix seconds); "newest" sorts by it.
    pub created: i64,
    pub container: Container,
    pub codec: Codec,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_no: u16,
    pub disc_no: u16,
    pub year: u16,
    pub genres: Vec<String>,
    pub compilation: bool,
    pub duration_ms: u64,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: u8,
    pub picture: Option<PictureRef>,
}

/// Where an entity's cover art comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cover {
    None,
    /// An image file, relative to the library root.
    Folder(String),
    /// The picture embedded in a track (by index).
    Embedded(u32),
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: Id,
    pub rec: TrackRecord,
    /// Display values with fallbacks applied.
    pub title: String,
    pub artist: String,
    pub album: u32,
    pub artist_ix: u32,
    pub dir: u32,
    /// Lowercase file extension.
    pub suffix: String,
    pub search: String,
}

#[derive(Clone, Debug)]
pub struct Album {
    pub id: Id,
    pub name: String,
    pub artist: String,
    pub artist_ix: u32,
    pub year: u16,
    pub genre: Option<String>,
    /// In disc, track, then title order.
    pub tracks: Vec<u32>,
    pub duration_ms: u64,
    pub created: i64,
    pub cover: Cover,
    pub dir: u32,
    pub sort: String,
    pub search: String,
}

#[derive(Clone, Debug)]
pub struct Artist {
    pub id: Id,
    pub name: String,
    /// Albums this artist is the album artist of, by year then name.
    pub albums: Vec<u32>,
    pub cover: Cover,
    pub sort: String,
    pub search: String,
}

#[derive(Clone, Debug)]
pub struct Dir {
    pub id: Id,
    /// Relative to the root; "" for the root itself.
    pub path: String,
    pub name: String,
    pub parent: Option<u32>,
    pub dirs: Vec<u32>,
    pub tracks: Vec<u32>,
    pub cover: Cover,
    pub sort: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Genre {
    pub name: String,
    pub songs: u32,
    pub albums: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entity {
    Track(u32),
    Album(u32),
    Artist(u32),
    Dir(u32),
}

#[derive(Debug, Default)]
pub struct Library {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub dirs: Vec<Dir>,
    pub genres: Vec<Genre>,
    /// Folder images found by the scan: directory path to image path.
    pub folder_covers: Vec<(String, String)>,
    pub(crate) by_id: HashMap<(Kind, [u8; ID_BYTES]), Entity>,
    /// Artists (with albums) in sort order.
    pub artists_sorted: Vec<u32>,
    /// Albums by name, by album artist then name, and newest first.
    pub albums_by_name: Vec<u32>,
    pub albums_by_artist: Vec<u32>,
    pub albums_by_created: Vec<u32>,
    /// When the scan that built this finished (Unix milliseconds).
    pub scanned_at_ms: u64,
}

impl Library {
    pub fn lookup(&self, id: &Id) -> Option<Entity> {
        self.by_id.get(&(id.kind, id.bytes)).copied()
    }

    pub fn track_by_id(&self, id: &Id) -> Option<&Track> {
        match self.lookup(id)? {
            Entity::Track(i) => self.tracks.get(i as usize),
            _ => None,
        }
    }

    pub fn album_by_id(&self, id: &Id) -> Option<&Album> {
        match self.lookup(id)? {
            Entity::Album(i) => self.albums.get(i as usize),
            _ => None,
        }
    }

    pub fn artist_by_id(&self, id: &Id) -> Option<&Artist> {
        match self.lookup(id)? {
            Entity::Artist(i) => self.artists.get(i as usize),
            _ => None,
        }
    }

    pub fn dir_by_id(&self, id: &Id) -> Option<&Dir> {
        match self.lookup(id)? {
            Entity::Dir(i) => self.dirs.get(i as usize),
            _ => None,
        }
    }

    pub fn track(&self, i: u32) -> &Track {
        &self.tracks[i as usize]
    }

    pub fn album(&self, i: u32) -> &Album {
        &self.albums[i as usize]
    }

    pub fn artist(&self, i: u32) -> &Artist {
        &self.artists[i as usize]
    }

    pub fn dir(&self, i: u32) -> &Dir {
        &self.dirs[i as usize]
    }

    /// The root directory (always index 0 in a built library).
    pub fn root(&self) -> Option<&Dir> {
        self.dirs.first()
    }

    pub fn records(&self) -> Vec<TrackRecord> {
        self.tracks.iter().map(|t| t.rec.clone()).collect()
    }
}
