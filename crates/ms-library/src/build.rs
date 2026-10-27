//! Turning scanned track records into a [`Library`]: the directory tree,
//! album grouping, artists, genres, covers, sorted views, and the lookup
//! table.
//!
//! Album grouping: tracks with an album-artist tag group by (album artist,
//! album). Tracks without one group by (directory, album), so an untagged
//! compilation in one folder stays one album instead of splitting per track
//! artist; its artist is the shared track artist, or "Various Artists".

use std::collections::HashMap;

use crate::fold::{fold, sort_key};
use crate::id::{Id, Kind};
use crate::model::{Album, Artist, Cover, Dir, Entity, Genre, Library, Track, TrackRecord};

pub const UNKNOWN_ARTIST: &str = "[Unknown Artist]";
pub const UNKNOWN_ALBUM: &str = "[Unknown Album]";
pub const VARIOUS_ARTISTS: &str = "Various Artists";
pub const ROOT_NAME: &str = "Music";

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s).trim()
}

/// Tracks that will become one album.
struct Group {
    key: Vec<String>,
    name: String,
    album_artist: Option<String>,
    compilation: bool,
    tracks: Vec<usize>,
}

struct Builder {
    lib: Library,
    dir_ix: HashMap<String, u32>,
    artist_ix: HashMap<String, u32>,
}

fn ix(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Builder {
    fn dir(&mut self, path: &str) -> u32 {
        if let Some(&i) = self.dir_ix.get(path) {
            return i;
        }
        let parent = if path.is_empty() { None } else { Some(self.dir(parent_of(path))) };
        let name = if path.is_empty() { ROOT_NAME.to_owned() } else { file_name(path).to_owned() };
        let i = ix(self.lib.dirs.len());
        self.lib.dirs.push(Dir {
            id: Id::derive(Kind::Dir, &[path.as_bytes()]),
            path: path.to_owned(),
            sort: sort_key(&name),
            name,
            parent,
            dirs: Vec::new(),
            tracks: Vec::new(),
            cover: Cover::None,
        });
        if let Some(p) = parent {
            self.lib.dirs[p as usize].dirs.push(i);
        }
        self.dir_ix.insert(path.to_owned(), i);
        i
    }

    fn artist(&mut self, name: &str) -> u32 {
        let key = fold(name);
        if let Some(&i) = self.artist_ix.get(&key) {
            return i;
        }
        let i = ix(self.lib.artists.len());
        self.lib.artists.push(Artist {
            id: Id::derive(Kind::Artist, &[key.as_bytes()]),
            name: name.to_owned(),
            albums: Vec::new(),
            cover: Cover::None,
            sort: sort_key(name),
            search: key.clone(),
        });
        self.artist_ix.insert(key, i);
        i
    }
}

/// Builds the library from the scanner's records and folder images.
pub fn build(
    mut records: Vec<TrackRecord>,
    folder_covers: Vec<(String, String)>,
    scanned_at_ms: u64,
) -> Library {
    records.sort_by(|a, b| a.path.cmp(&b.path));
    records.dedup_by(|a, b| a.path == b.path);
    let mut b =
        Builder { lib: Library::default(), dir_ix: HashMap::new(), artist_ix: HashMap::new() };
    b.dir("");
    let covers: HashMap<String, String> = folder_covers.iter().cloned().collect();
    b.lib.folder_covers = folder_covers;
    b.lib.scanned_at_ms = scanned_at_ms;

    // Group tracks into albums.
    let mut groups: Vec<Group> = Vec::new();
    let mut group_ix: HashMap<Vec<String>, usize> = HashMap::new();
    let mut track_dirs = Vec::with_capacity(records.len());
    for (n, r) in records.iter().enumerate() {
        let dir_path = parent_of(&r.path);
        let dir = b.dir(dir_path);
        track_dirs.push(dir);
        let name = r.album.clone().unwrap_or_else(|| {
            if dir_path.is_empty() {
                UNKNOWN_ALBUM.to_owned()
            } else {
                file_name(dir_path).to_owned()
            }
        });
        let key = match &r.album_artist {
            Some(aa) => vec!["artist".to_owned(), fold(aa), fold(&name)],
            None => vec!["dir".to_owned(), dir_path.to_owned(), fold(&name)],
        };
        let g = *group_ix.entry(key.clone()).or_insert_with(|| {
            groups.push(Group {
                key,
                name,
                album_artist: r.album_artist.clone(),
                compilation: false,
                tracks: Vec::new(),
            });
            groups.len() - 1
        });
        groups[g].tracks.push(n);
        groups[g].compilation |= r.compilation;
    }

    // Tracks.
    let mut track_album = vec![0_u32; records.len()];
    for (g_ix, g) in groups.iter().enumerate() {
        for &t in &g.tracks {
            track_album[t] = ix(g_ix);
        }
    }
    let album_artists: Vec<String> = groups
        .iter()
        .map(|g| {
            if let Some(aa) = &g.album_artist {
                return aa.clone();
            }
            let first = records[g.tracks[0]].artist.as_deref();
            let shared =
                g.tracks.iter().all(|&t| records[t].artist.as_deref().map(fold) == first.map(fold));
            match first {
                Some(a) if shared && !g.compilation => a.to_owned(),
                None if shared => UNKNOWN_ARTIST.to_owned(),
                _ => VARIOUS_ARTISTS.to_owned(),
            }
        })
        .collect();
    for (n, r) in records.into_iter().enumerate() {
        let album = track_album[n];
        let title = r.title.clone().unwrap_or_else(|| stem(file_name(&r.path)).to_owned());
        let artist = r.artist.clone().unwrap_or_else(|| album_artists[album as usize].clone());
        let artist_ix = b.artist(&artist);
        let suffix = file_name(&r.path)
            .rsplit_once('.')
            .map_or(String::new(), |(_, e)| e.to_ascii_lowercase());
        let search = fold(&format!("{title} {artist} {}", groups[album as usize].name));
        let i = ix(b.lib.tracks.len());
        b.lib.dirs[track_dirs[n] as usize].tracks.push(i);
        b.lib.tracks.push(Track {
            id: Id::derive(Kind::Track, &[r.path.as_bytes()]),
            title,
            artist,
            album,
            artist_ix,
            dir: track_dirs[n],
            suffix,
            search,
            rec: r,
        });
    }

    // Albums.
    for (g_ix, g) in groups.iter().enumerate() {
        let artist_name = &album_artists[g_ix];
        let artist_ix = b.artist(artist_name);
        let mut tracks: Vec<u32> = g.tracks.iter().map(|&t| ix(t)).collect();
        let lib = &b.lib;
        tracks.sort_by(|&x, &y| {
            let (x, y) = (&lib.tracks[x as usize], &lib.tracks[y as usize]);
            (x.rec.disc_no, x.rec.track_no, fold(&x.title), &x.rec.path).cmp(&(
                y.rec.disc_no,
                y.rec.track_no,
                fold(&y.title),
                &y.rec.path,
            ))
        });
        let first = &lib.tracks[tracks[0] as usize];
        let dir = first.dir;
        let dir_path = &lib.dirs[dir as usize].path;
        let cover = match covers.get(dir_path) {
            Some(img) => Cover::Folder(img.clone()),
            None => tracks
                .iter()
                .find(|&&t| lib.tracks[t as usize].rec.picture.is_some())
                .map_or(Cover::None, |&t| Cover::Embedded(t)),
        };
        let records = tracks.iter().map(|&t| &lib.tracks[t as usize].rec);
        let year = records.clone().map(|r| r.year).find(|&y| y > 0).unwrap_or(0);
        let genre = records.clone().find_map(|r| r.genres.first().cloned());
        let duration_ms = records.clone().map(|r| r.duration_ms).sum();
        let created = records.map(|r| r.created).min().unwrap_or(0);
        let key: Vec<&[u8]> = g.key.iter().map(String::as_bytes).collect();
        let album = Album {
            id: Id::derive(Kind::Album, &key),
            name: g.name.clone(),
            artist: artist_name.clone(),
            artist_ix,
            year,
            genre,
            tracks,
            duration_ms,
            created,
            cover,
            dir,
            sort: sort_key(&g.name),
            search: fold(&format!("{} {artist_name}", g.name)),
        };
        let a_ix = ix(b.lib.albums.len());
        b.lib.artists[artist_ix as usize].albums.push(a_ix);
        b.lib.albums.push(album);
    }

    let mut lib = b.lib;
    // Artist album order and covers.
    for artist in &mut lib.artists {
        let albums = &lib.albums;
        artist.albums.sort_by(|&x, &y| {
            let (x, y) = (&albums[x as usize], &albums[y as usize]);
            (x.year, &x.sort).cmp(&(y.year, &y.sort))
        });
        artist.cover = artist
            .albums
            .iter()
            .rev()
            .map(|&a| &albums[a as usize].cover)
            .find(|c| **c != Cover::None)
            .cloned()
            .unwrap_or(Cover::None);
    }
    // Directory order and covers.
    for d in 0..lib.dirs.len() {
        let mut dirs = std::mem::take(&mut lib.dirs[d].dirs);
        dirs.sort_by(|&x, &y| lib.dirs[x as usize].sort.cmp(&lib.dirs[y as usize].sort));
        let mut tracks = std::mem::take(&mut lib.dirs[d].tracks);
        tracks.sort_by(|&x, &y| {
            let (x, y) = (&lib.tracks[x as usize], &lib.tracks[y as usize]);
            (x.rec.disc_no, x.rec.track_no, &x.rec.path).cmp(&(
                y.rec.disc_no,
                y.rec.track_no,
                &y.rec.path,
            ))
        });
        let cover = match covers.get(&lib.dirs[d].path) {
            Some(img) => Cover::Folder(img.clone()),
            None => tracks.first().map_or(Cover::None, |&t| {
                lib.albums[lib.tracks[t as usize].album as usize].cover.clone()
            }),
        };
        let dir = &mut lib.dirs[d];
        dir.dirs = dirs;
        dir.tracks = tracks;
        dir.cover = cover;
    }
    // Genres.
    let mut genres: HashMap<String, Genre> = HashMap::new();
    for t in &lib.tracks {
        for g in &t.rec.genres {
            genres
                .entry(fold(g))
                .or_insert_with(|| Genre { name: g.clone(), songs: 0, albums: 0 })
                .songs += 1;
        }
    }
    for a in &lib.albums {
        if let Some(g) = &a.genre
            && let Some(entry) = genres.get_mut(&fold(g))
        {
            entry.albums += 1;
        }
    }
    let mut genres: Vec<Genre> = genres.into_values().collect();
    genres.sort_by_key(|g| fold(&g.name));
    lib.genres = genres;
    // Sorted views.
    let mut artists: Vec<u32> = (0..ix(lib.artists.len()))
        .filter(|&a| !lib.artists[a as usize].albums.is_empty())
        .collect();
    artists.sort_by(|&x, &y| lib.artists[x as usize].sort.cmp(&lib.artists[y as usize].sort));
    lib.artists_sorted = artists;
    let all: Vec<u32> = (0..ix(lib.albums.len())).collect();
    let mut by_name = all.clone();
    by_name.sort_by(|&x, &y| lib.albums[x as usize].sort.cmp(&lib.albums[y as usize].sort));
    let mut by_artist = all.clone();
    by_artist.sort_by(|&x, &y| {
        let (x, y) = (&lib.albums[x as usize], &lib.albums[y as usize]);
        (&lib.artists[x.artist_ix as usize].sort, x.year, &x.sort).cmp(&(
            &lib.artists[y.artist_ix as usize].sort,
            y.year,
            &y.sort,
        ))
    });
    let mut by_created = all;
    by_created.sort_by(|&x, &y| {
        let (x, y) = (&lib.albums[x as usize], &lib.albums[y as usize]);
        y.created.cmp(&x.created).then_with(|| x.sort.cmp(&y.sort))
    });
    lib.albums_by_name = by_name;
    lib.albums_by_artist = by_artist;
    lib.albums_by_created = by_created;
    // Lookup.
    let mut by_id = HashMap::with_capacity(
        lib.tracks.len() + lib.albums.len() + lib.artists.len() + lib.dirs.len(),
    );
    for (i, t) in lib.tracks.iter().enumerate() {
        by_id.insert((t.id.kind, t.id.bytes), Entity::Track(ix(i)));
    }
    for (i, a) in lib.albums.iter().enumerate() {
        by_id.insert((a.id.kind, a.id.bytes), Entity::Album(ix(i)));
    }
    for (i, a) in lib.artists.iter().enumerate() {
        by_id.insert((a.id.kind, a.id.bytes), Entity::Artist(ix(i)));
    }
    for (i, d) in lib.dirs.iter().enumerate() {
        by_id.insert((d.id.kind, d.id.bytes), Entity::Dir(ix(i)));
    }
    lib.by_id = by_id;
    lib
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ms_tags::{Codec, Container};

    pub(crate) fn rec(
        path: &str,
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        aa: Option<&str>,
        track: u16,
    ) -> TrackRecord {
        TrackRecord {
            path: path.to_owned(),
            size: 1000,
            mtime_ns: 1,
            created: i64::from(track),
            container: Container::Mp3,
            codec: Codec::Mp3,
            title: title.map(str::to_owned),
            artist: artist.map(str::to_owned),
            album: album.map(str::to_owned),
            album_artist: aa.map(str::to_owned),
            track_no: track,
            disc_no: 1,
            year: 2000,
            genres: vec!["Rock".to_owned()],
            compilation: false,
            duration_ms: 1000,
            bitrate_kbps: 128,
            sample_rate: 44_100,
            channels: 2,
            bit_depth: 0,
            picture: None,
        }
    }

    #[test]
    fn grouping_fallbacks_and_views() {
        let lib = build(
            vec![
                rec("B/Album/02.mp3", Some("Two"), Some("The Band"), Some("Album"), None, 2),
                rec("B/Album/01.mp3", Some("One"), Some("The Band"), Some("Album"), None, 1),
                rec("Comp/01.mp3", Some("X"), Some("Artist X"), Some("Hits"), None, 1),
                rec("Comp/02.mp3", Some("Y"), Some("Artist Y"), Some("Hits"), None, 2),
                rec("Other/Hits/01.mp3", Some("Z"), Some("Zed"), Some("Hits"), Some("Zed"), 1),
                rec("loose.mp3", None, None, None, None, 0),
            ],
            vec![("B/Album".to_owned(), "B/Album/cover.jpg".to_owned())],
            42,
        );
        assert_eq!(lib.tracks.len(), 6);
        assert_eq!(lib.albums.len(), 4);
        let album = |name: &str, artist: &str| {
            lib.albums.iter().find(|a| a.name == name && a.artist == artist).unwrap()
        };
        let band = album("Album", "The Band");
        let titles: Vec<&str> = band.tracks.iter().map(|&t| lib.track(t).title.as_str()).collect();
        assert_eq!(titles, ["One", "Two"]);
        assert_eq!(band.cover, Cover::Folder("B/Album/cover.jpg".to_owned()));
        assert_eq!(band.duration_ms, 2000);
        assert_eq!(album("Hits", VARIOUS_ARTISTS).tracks.len(), 2);
        assert_eq!(album("Hits", "Zed").tracks.len(), 1);
        let loose = album(UNKNOWN_ALBUM, UNKNOWN_ARTIST);
        assert_eq!(lib.track(loose.tracks[0]).title, "loose");
        // Directory tree.
        let root = lib.root().unwrap();
        let names: Vec<&str> = root.dirs.iter().map(|&d| lib.dir(d).name.as_str()).collect();
        assert_eq!(names, ["B", "Comp", "Other"]);
        assert_eq!(root.tracks.len(), 1);
        // Artists with albums, ignoring "The".
        let artists: Vec<&str> =
            lib.artists_sorted.iter().map(|&a| lib.artist(a).name.as_str()).collect();
        assert_eq!(artists, [UNKNOWN_ARTIST, "The Band", VARIOUS_ARTISTS, "Zed"]);
        // Every entity resolves by its id.
        for t in &lib.tracks {
            assert_eq!(lib.track_by_id(&t.id).unwrap().rec.path, t.rec.path);
        }
        for a in &lib.albums {
            assert_eq!(lib.album_by_id(&a.id).unwrap().name, a.name);
        }
        assert_eq!(lib.genres, [Genre { name: "Rock".into(), songs: 6, albums: 4 }]);
        assert_eq!(lib.scanned_at_ms, 42);
    }

    #[test]
    fn ids_survive_a_rebuild() {
        let records = vec![rec("a/b.mp3", Some("t"), Some("x"), Some("y"), None, 1)];
        let one = build(records.clone(), vec![], 1);
        let two = build(records, vec![], 2);
        assert_eq!(one.tracks[0].id, two.tracks[0].id);
        assert_eq!(one.albums[0].id, two.albums[0].id);
        assert_eq!(one.artists[0].id, two.artists[0].id);
    }
}
