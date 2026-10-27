//! Library entities as Subsonic elements.
//!
//! A [`View`] pairs a library snapshot with the caller's own data, so every
//! element carries that user's play count and star.

use ms_common::timefmt::rfc3339_millis;
use ms_library::{Cover, Library};
use ms_store::{Playlist, UserData};
use ms_tags::Container;

use crate::encode::Node;

/// Seconds, rounded to the nearest second, at least 1 for a non-empty
/// track (clients divide by it).
pub fn seconds(ms: u64) -> u64 {
    if ms == 0 { 0 } else { (ms + 500) / 1000 }.max(u64::from(ms > 0))
}

pub fn iso(seconds: i64) -> String {
    rfc3339_millis(seconds, 0)
}

pub fn iso_ms(ms: i64) -> String {
    rfc3339_millis(ms.div_euclid(1000), u32::try_from(ms.rem_euclid(1000)).unwrap_or(0))
}

/// The MIME type a file is served as.
pub fn content_type(container: Container) -> &'static str {
    match container {
        Container::Mp3 => "audio/mpeg",
        Container::Flac => "audio/flac",
        Container::Mp4 => "audio/mp4",
        Container::Ogg => "audio/ogg",
        Container::Wav => "audio/wav",
    }
}

#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    pub lib: &'a Library,
    pub ud: &'a UserData,
}

impl View<'_> {
    fn starred(&self, id: &str) -> Option<String> {
        self.ud.starred_at(id).map(iso_ms)
    }

    fn album_cover(&self, album: u32) -> Option<String> {
        let a = self.lib.album(album);
        (a.cover != Cover::None).then(|| a.id.to_string())
    }

    /// A track: `<song>`, `<child>`, or `<entry>` depending on the call.
    pub fn song(&self, name: &'static str, t: u32) -> Node {
        let track = self.lib.track(t);
        let r = &track.rec;
        let id = track.id.to_string();
        let album = self.lib.album(track.album);
        let play = self.ud.plays.get(&id);
        let mut n = Node::new(name)
            .s("id", id.clone())
            .s("parent", self.lib.dir(track.dir).id.to_string())
            .b("isDir", false)
            .s("title", track.title.clone())
            .s("album", album.name.clone())
            .s("artist", track.artist.clone())
            .nz("track", u64::from(r.track_no))
            .nz("year", u64::from(r.year))
            .opt_s("genre", r.genres.first().map(String::as_str))
            .opt_s("coverArt", self.album_cover(track.album).as_deref())
            .u("size", r.size)
            .s("contentType", content_type(r.container))
            .s("suffix", track.suffix.clone())
            .u("duration", seconds(r.duration_ms))
            .nz("bitRate", u64::from(r.bitrate_kbps))
            .s("path", r.path.clone())
            .nz("discNumber", u64::from(r.disc_no))
            .s("created", iso(r.created))
            .s("albumId", album.id.to_string())
            .s("artistId", self.lib.artist(track.artist_ix).id.to_string())
            .s("type", "music")
            .b("isVideo", false);
        if let Some(p) = play {
            n = n.u("playCount", p.count).s("played", iso_ms(p.last));
        }
        if let Some(s) = self.starred(&id) {
            n = n.s("starred", s);
        }
        n.nz("samplingRate", u64::from(r.sample_rate))
            .nz("channelCount", u64::from(r.channels))
            .nz("bitDepth", u64::from(r.bit_depth))
            .s("mediaType", "song")
            .s("displayArtist", track.artist.clone())
            .s("displayAlbumArtist", album.artist.clone())
    }

    fn album_plays(&self, album: u32) -> (u64, i64) {
        let mut count = 0;
        let mut last = 0;
        for &t in &self.lib.album(album).tracks {
            if let Some(p) = self.ud.plays.get(&self.lib.track(t).id.to_string()) {
                count += p.count;
                last = last.max(p.last);
            }
        }
        (count, last)
    }

    /// An album in the ID3 views (`getAlbum`, `getAlbumList2`, `search3`).
    pub fn album(&self, name: &'static str, a: u32) -> Node {
        let album = self.lib.album(a);
        let id = album.id.to_string();
        let (plays, last) = self.album_plays(a);
        let mut n = Node::new(name)
            .s("id", id.clone())
            .s("name", album.name.clone())
            .s("artist", album.artist.clone())
            .s("artistId", self.lib.artist(album.artist_ix).id.to_string())
            .opt_s("coverArt", self.album_cover(a).as_deref())
            .u("songCount", album.tracks.len() as u64)
            .u("duration", seconds(album.duration_ms))
            .s("created", iso(album.created))
            .nz("year", u64::from(album.year))
            .opt_s("genre", album.genre.as_deref());
        if plays > 0 {
            n = n.u("playCount", plays).s("played", iso_ms(last));
        }
        if let Some(s) = self.starred(&id) {
            n = n.s("starred", s);
        }
        n.s("displayArtist", album.artist.clone())
    }

    /// An album as a folder-style entry (`getAlbumList`, `getStarred`,
    /// an artist's `getMusicDirectory`).
    pub fn album_dir(&self, name: &'static str, a: u32) -> Node {
        let album = self.lib.album(a);
        let id = album.id.to_string();
        let mut n = Node::new(name)
            .s("id", id.clone())
            .s("parent", self.lib.artist(album.artist_ix).id.to_string())
            .b("isDir", true)
            .s("title", album.name.clone())
            .s("name", album.name.clone())
            .s("album", album.name.clone())
            .s("artist", album.artist.clone())
            .nz("year", u64::from(album.year))
            .opt_s("genre", album.genre.as_deref())
            .opt_s("coverArt", self.album_cover(a).as_deref())
            .u("duration", seconds(album.duration_ms))
            .u("songCount", album.tracks.len() as u64)
            .s("created", iso(album.created));
        let (plays, _) = self.album_plays(a);
        if plays > 0 {
            n = n.u("playCount", plays);
        }
        if let Some(s) = self.starred(&id) {
            n = n.s("starred", s);
        }
        n
    }

    pub fn artist(&self, name: &'static str, a: u32) -> Node {
        let artist = self.lib.artist(a);
        let id = artist.id.to_string();
        let mut n = Node::new(name).s("id", id.clone()).s("name", artist.name.clone());
        if artist.cover != Cover::None {
            n = n.s("coverArt", id.clone());
        }
        n = n.u("albumCount", artist.albums.len() as u64);
        if let Some(s) = self.starred(&id) {
            n = n.s("starred", s);
        }
        n
    }

    /// A directory inside a folder listing.
    pub fn dir_child(&self, d: u32) -> Node {
        let dir = self.lib.dir(d);
        let id = dir.id.to_string();
        let mut n = Node::new("child").s("id", id.clone());
        if let Some(p) = dir.parent {
            n = n.s("parent", self.lib.dir(p).id.to_string());
        }
        n = n.b("isDir", true).s("title", dir.name.clone());
        if dir.cover != Cover::None {
            n = n.s("coverArt", id.clone());
        }
        if let Some(s) = self.starred(&id) {
            n = n.s("starred", s);
        }
        n
    }

    /// The playable tracks of a playlist, in order.
    pub fn playlist_tracks(&self, p: &Playlist) -> Vec<u32> {
        p.entries
            .iter()
            .filter_map(|e| ms_library::Id::parse(e).ok())
            .filter_map(|id| match self.lib.lookup(&id) {
                Some(ms_library::Entity::Track(t)) => Some(t),
                _ => None,
            })
            .collect()
    }

    pub fn playlist(&self, owner: &str, p: &Playlist, with_entries: bool) -> Node {
        let tracks = self.playlist_tracks(p);
        let duration: u64 = tracks.iter().map(|&t| self.lib.track(t).rec.duration_ms).sum();
        let mut n = Node::new("playlist")
            .s("id", p.id.clone())
            .s("name", p.name.clone())
            .s("comment", p.comment.clone())
            .s("owner", owner)
            .b("public", p.public)
            .u("songCount", tracks.len() as u64)
            .u("duration", seconds(duration))
            .s("created", iso_ms(p.created))
            .s("changed", iso_ms(p.changed));
        if let Some(&first) = tracks.first()
            && self.lib.album(self.lib.track(first).album).cover != Cover::None
        {
            n = n.s("coverArt", p.id.clone());
        }
        if with_entries {
            n = n.list("entry", tracks.iter().map(|&t| self.song("entry", t)).collect());
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_times() {
        assert_eq!(seconds(0), 0);
        assert_eq!(seconds(200), 1);
        assert_eq!(seconds(1499), 1);
        assert_eq!(seconds(1500), 2);
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_ms(1500), "1970-01-01T00:00:01.500Z");
        assert_eq!(content_type(Container::Ogg), "audio/ogg");
    }
}
