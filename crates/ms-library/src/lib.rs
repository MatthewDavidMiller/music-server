//! The music library: a scan of the music directory into an immutable,
//! in-memory index of tracks, albums, artists, directories, and genres,
//! with stable identifiers, search, and an on-disk cache of the scan.
//!
//! There is no database. A library of 50 000 tracks is a few tens of
//! megabytes in memory and rebuilds from the cache in well under a second.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod build;
pub mod cache;
pub mod fold;
pub mod id;
pub mod model;
pub mod scan;
pub mod search;
pub mod service;

pub use id::{Id, Kind};
pub use model::{Album, Artist, Cover, Dir, Entity, Genre, Library, Track, TrackRecord};
pub use search::Query;
pub use service::{LibraryConfig, LibraryService, ScanStatus};
