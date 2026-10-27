//! Audio metadata, read by hand: the tags a library is organised by (title,
//! artist, album, track and disc numbers, year, genres), the stream facts a
//! player needs (duration, bitrate, sample rate, channels), and where the
//! embedded cover art is.
//!
//! | Container | Tags | Stream facts |
//! |---|---|---|
//! | MP3 | ID3v2.2/2.3/2.4, ID3v1 fallback | MPEG frame headers, Xing/Info/VBRI |
//! | FLAC | Vorbis comments, PICTURE blocks | STREAMINFO |
//! | Ogg Vorbis, Ogg Opus | Vorbis comments, METADATA_BLOCK_PICTURE | id header, last granule |
//! | MP4 (AAC, ALAC) | iTunes `ilst` atoms | `mvhd`/`mdhd`, `stsd`, `esds` |
//! | WAV | `LIST/INFO`, an `id3 ` chunk | `fmt `, `data` |
//!
//! Everything reads through [`Source`], which hands out bounded slices of a
//! file, so cover art is skipped by seeking rather than read: a scan of a
//! large library touches the header region of each file and little else.
//! The container is recognised by its magic bytes, not its file name.
//!
//! Every parser treats its input as hostile: lengths are checked before they
//! are used, counts and sizes are capped (see [`limits`]), and a file that
//! cannot be understood yields an error the scanner records, never a panic.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))]

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};

mod flac;
mod genre;
mod id3v1;
mod id3v2;
pub mod image;
mod mp4;
mod mpeg;
mod ogg;
mod text;
mod vorbis;
mod wav;

#[cfg(any(test, feature = "testgen"))]
pub mod testgen;

pub use ms_common::{Error, Result};

/// Bumped whenever a parser change would read an existing file differently,
/// so a library index built by an older parser is rebuilt rather than kept.
pub const PARSER_VERSION: u32 = 1;

pub mod limits {
    /// The most metadata read from one file (tag blocks, atoms, packets).
    pub const MAX_METADATA: usize = 16 * 1024 * 1024;
    /// The most one tag frame or comment may occupy before it is skipped.
    pub const MAX_FIELD: usize = 1024 * 1024;
    /// Text values are cut to this many bytes.
    pub const MAX_TEXT: usize = 4096;
    pub const MAX_GENRES: usize = 64;
    /// Frames, atoms, comments, blocks, or pages walked in one structure.
    pub const MAX_ITEMS: usize = 4096;
    /// The largest embedded picture served.
    pub const MAX_PICTURE: usize = 20 * 1024 * 1024;
}

/// A file's container, from its leading bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    Mp3,
    Flac,
    Ogg,
    Mp4,
    Wav,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Codec {
    Mp3,
    Mp2,
    Mp1,
    Flac,
    Vorbis,
    Opus,
    Aac,
    Alac,
    Pcm,
    #[default]
    Unknown,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Mp3 => "mp3",
            Codec::Mp2 => "mp2",
            Codec::Mp1 => "mp1",
            Codec::Flac => "flac",
            Codec::Vorbis => "vorbis",
            Codec::Opus => "opus",
            Codec::Aac => "aac",
            Codec::Alac => "alac",
            Codec::Pcm => "pcm",
            Codec::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<u16>,
    pub track_total: Option<u16>,
    pub disc: Option<u16>,
    pub disc_total: Option<u16>,
    pub year: Option<u16>,
    pub genres: Vec<String>,
    pub compilation: bool,
}

impl Tags {
    /// Fills every field still empty here from `other`.
    pub fn fill_from(&mut self, other: Tags) {
        fn take<T>(slot: &mut Option<T>, value: Option<T>) {
            if slot.is_none() {
                *slot = value;
            }
        }
        take(&mut self.title, other.title);
        take(&mut self.artist, other.artist);
        take(&mut self.album, other.album);
        take(&mut self.album_artist, other.album_artist);
        take(&mut self.track, other.track);
        take(&mut self.track_total, other.track_total);
        take(&mut self.disc, other.disc);
        take(&mut self.disc_total, other.disc_total);
        take(&mut self.year, other.year);
        if self.genres.is_empty() {
            self.genres = other.genres;
        }
        self.compilation |= other.compilation;
    }

    fn add_genre(&mut self, genre: &str) {
        let g = text::clean(genre);
        if let Some(g) = g
            && self.genres.len() < limits::MAX_GENRES
            && !self.genres.iter().any(|x| x.eq_ignore_ascii_case(&g))
        {
            self.genres.push(g);
        }
    }

    /// Applies one textual field by its conventional (Vorbis comment) name.
    /// Unknown names are ignored. The first value of a single-valued field
    /// wins.
    fn set_named(&mut self, key: &str, value: &str) {
        let set = |slot: &mut Option<String>| {
            if slot.is_none() {
                *slot = text::clean(value);
            }
        };
        match key.to_ascii_uppercase().as_str() {
            "TITLE" => set(&mut self.title),
            "ARTIST" => set(&mut self.artist),
            "ALBUM" => set(&mut self.album),
            "ALBUMARTIST" | "ALBUM ARTIST" | "ALBUM_ARTIST" => set(&mut self.album_artist),
            "TRACKNUMBER" | "TRACK" => {
                let (n, total) = text::number_pair(value);
                self.track = self.track.or(n);
                self.track_total = self.track_total.or(total);
            }
            "TRACKTOTAL" | "TOTALTRACKS" => {
                self.track_total = self.track_total.or(text::number_pair(value).0);
            }
            "DISCNUMBER" | "DISC" => {
                let (n, total) = text::number_pair(value);
                self.disc = self.disc.or(n);
                self.disc_total = self.disc_total.or(total);
            }
            "DISCTOTAL" | "TOTALDISCS" => {
                self.disc_total = self.disc_total.or(text::number_pair(value).0);
            }
            "DATE" | "YEAR" | "ORIGINALDATE" | "ORIGINALYEAR" => {
                self.year = self.year.or(text::year(value));
            }
            "GENRE" => {
                for g in value.split([';', '\0']) {
                    self.add_genre(g);
                }
            }
            "COMPILATION" => self.compilation |= text::truthy(value),
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioInfo {
    pub duration_ms: u64,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Bits per sample for lossless codecs; 0 when it does not apply.
    pub bit_depth: u8,
    pub codec: Codec,
}

/// Where a file's cover picture is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PictureRef {
    /// The image bytes, stored contiguously and unencoded in the file.
    Slice { offset: u64, len: u32, mime: Mime },
    /// Present, but stored transformed (ID3 unsynchronisation, base64 in a
    /// Vorbis comment); [`load_picture`] parses the file again to get it.
    Reparse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mime {
    Jpeg,
    Png,
    Gif,
    Bmp,
    Webp,
    Other,
}

impl Mime {
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "image/jpeg" | "image/jpg" | "jpg" | "jpeg" => Mime::Jpeg,
            "image/png" | "png" => Mime::Png,
            "image/gif" | "gif" => Mime::Gif,
            "image/bmp" | "bmp" => Mime::Bmp,
            "image/webp" => Mime::Webp,
            _ => Mime::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mime::Jpeg => "image/jpeg",
            Mime::Png => "image/png",
            Mime::Gif => "image/gif",
            Mime::Bmp => "image/bmp",
            Mime::Webp => "image/webp",
            Mime::Other => "application/octet-stream",
        }
    }
}

/// A decoded picture: its bytes and their type (sniffed when the file's own
/// claim was missing or generic).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub data: Vec<u8>,
    pub mime: Mime,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    pub container: Option<Container>,
    pub tags: Tags,
    pub audio: AudioInfo,
    pub picture: Option<PictureRef>,
}

/// Random access to a file, in bounded pieces.
pub trait Source {
    fn size(&self) -> u64;
    /// Up to `len` bytes at `offset`; fewer only at the end of the file.
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>>;
}

/// A [`Source`] over an open file.
#[derive(Debug)]
pub struct FileSource {
    file: File,
    size: u64,
}

impl FileSource {
    pub fn new(file: File) -> Result<Self> {
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl Source for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if len > limits::MAX_METADATA.max(limits::MAX_PICTURE) {
            return Err(Error::new("read exceeds its size limit"));
        }
        let available = self.size.saturating_sub(offset);
        let len = usize::try_from(available).map_or(len, |a| a.min(len));
        let mut out = Vec::new();
        out.try_reserve_exact(len).map_err(|_| Error::new("out of memory"))?;
        out.resize(len, 0);
        self.file.seek(SeekFrom::Start(offset))?;
        let mut done = 0;
        while done < len {
            match self.file.read(&mut out[done..]) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        out.truncate(done);
        Ok(out)
    }
}

/// A [`Source`] over bytes already in memory (an embedded ID3 chunk, tests).
#[derive(Debug)]
pub struct SliceSource<'a>(pub &'a [u8]);

impl Source for SliceSource<'_> {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(self.0.len());
        let end = start.saturating_add(len).min(self.0.len());
        Ok(self.0[start..end].to_vec())
    }
}

/// Recognises the container from the first bytes of a file.
pub fn sniff(src: &mut dyn Source) -> Result<Option<Container>> {
    let head = src.read_at(0, 12)?;
    Ok(sniff_bytes(&head).or_else(|| {
        // An ID3v2 tag may precede FLAC as well as MPEG audio.
        if head.starts_with(b"ID3") { id3v2::container_after_tag(src) } else { None }
    }))
}

fn sniff_bytes(head: &[u8]) -> Option<Container> {
    if head.starts_with(b"fLaC") {
        Some(Container::Flac)
    } else if head.starts_with(b"OggS") {
        Some(Container::Ogg)
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        Some(Container::Wav)
    } else if head.len() >= 8 && &head[4..8] == b"ftyp" {
        Some(Container::Mp4)
    } else if head.len() >= 2 && head[0] == 0xff && head[1] & 0xe0 == 0xe0 {
        Some(Container::Mp3)
    } else {
        None
    }
}

/// Everything the library needs from one file.
pub fn probe(src: &mut dyn Source) -> Result<Probe> {
    let container = sniff(src)?.ok_or_else(|| Error::new("not a recognised audio file"))?;
    let mut probe = match container {
        Container::Mp3 => mpeg::probe(src)?,
        Container::Flac => flac::probe(src)?,
        Container::Ogg => ogg::probe(src)?,
        Container::Mp4 => mp4::probe(src)?,
        Container::Wav => wav::probe(src)?,
    };
    probe.container = Some(container);
    Ok(probe)
}

/// The file's cover picture, wherever [`probe`] found it.
pub fn load_picture(src: &mut dyn Source, at: PictureRef) -> Result<Option<Picture>> {
    let found = match at {
        PictureRef::Slice { offset, len, mime } => {
            let len = usize::try_from(len).unwrap_or(usize::MAX);
            if len > limits::MAX_PICTURE {
                return Err(Error::new("picture exceeds its size limit"));
            }
            let data = src.read_at(offset, len)?;
            if data.len() != len {
                return Err(Error::new("picture is truncated"));
            }
            Some(Picture { data, mime })
        }
        PictureRef::Reparse => match sniff(src)? {
            Some(Container::Mp3) => id3v2::picture(src, 0)?,
            Some(Container::Flac) => flac::picture(src)?,
            Some(Container::Ogg) => ogg::picture(src)?,
            Some(Container::Wav) => wav::picture(src)?,
            Some(Container::Mp4) | None => None,
        },
    };
    Ok(found.map(|mut p| {
        if matches!(p.mime, Mime::Other | Mime::Jpeg | Mime::Png)
            && let Some(sniffed) = image::sniff(&p.data)
        {
            p.mime = sniffed;
        }
        p
    }))
}

/// Duration in milliseconds from a sample count and rate.
fn samples_to_ms(samples: u64, rate: u32) -> u64 {
    if rate == 0 {
        0
    } else {
        u64::try_from(u128::from(samples) * 1000 / u128::from(rate)).unwrap_or(u64::MAX)
    }
}

/// Average bitrate in kbit/s of `bytes` over `ms` milliseconds.
fn average_kbps(bytes: u64, ms: u64) -> u32 {
    if ms == 0 {
        return 0;
    }
    // bytes * 8 / (ms / 1000) / 1000 == bytes * 8 / ms
    u32::try_from(bytes.saturating_mul(8) / ms).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing() {
        assert_eq!(sniff_bytes(b"fLaC\0\0\0\0"), Some(Container::Flac));
        assert_eq!(sniff_bytes(b"OggS\0"), Some(Container::Ogg));
        assert_eq!(sniff_bytes(b"RIFF\0\0\0\0WAVE"), Some(Container::Wav));
        assert_eq!(sniff_bytes(b"\0\0\0\x20ftypM4A "), Some(Container::Mp4));
        assert_eq!(sniff_bytes(&[0xff, 0xfb, 0x90, 0x00]), Some(Container::Mp3));
        assert_eq!(sniff_bytes(b"hello world!"), None);
        assert_eq!(sniff_bytes(b""), None);
    }

    #[test]
    fn named_fields() {
        let mut t = Tags::default();
        t.set_named("title", " Song ");
        t.set_named("TITLE", "Second");
        t.set_named("TrackNumber", "3/12");
        t.set_named("DISCNUMBER", "2");
        t.set_named("DISCTOTAL", "3");
        t.set_named("DATE", "1999-04-01");
        t.set_named("GENRE", "Rock; Pop");
        t.set_named("GENRE", "rock");
        t.set_named("COMPILATION", "1");
        t.set_named("ALBUM ARTIST", "Various");
        assert_eq!(t.title.as_deref(), Some("Song"));
        assert_eq!((t.track, t.track_total), (Some(3), Some(12)));
        assert_eq!((t.disc, t.disc_total), (Some(2), Some(3)));
        assert_eq!(t.year, Some(1999));
        assert_eq!(t.genres, ["Rock", "Pop"]);
        assert!(t.compilation);
        assert_eq!(t.album_artist.as_deref(), Some("Various"));
    }

    /// Every fixture the builders make, for the robustness sweeps.
    fn fixtures() -> Vec<Vec<u8>> {
        use testgen::*;
        let pic = flac_picture(3, "image/png", &tiny_png(4, 4));
        let comments = vorbis_comments(&[
            "TITLE=t".into(),
            format!("METADATA_BLOCK_PICTURE={}", ms_common::base64::encode(&pic)),
        ]);
        let tag = id3v2(
            4,
            &[Id3Frame::text("TIT2", "t"), Id3Frame::picture(3, "image/png", &tiny_png(2, 2))],
            false,
        );
        let mut mp3 = tag.clone();
        mp3.extend(mpeg_frames(4, true));
        let mut unsynced = id3v2(
            3,
            &[Id3Frame::text("TIT2", "u"), Id3Frame::picture(3, "image/jpeg", &[0xff; 9])],
            true,
        );
        unsynced.extend(mpeg_frames(3, false));
        vec![
            mp3,
            unsynced,
            flac(44_100, 2, 16, 44_100, &comments, Some(&pic), 64),
            ogg_stream(true, &comments, 48_000, 0),
            ogg_stream(false, &comments, 44_100, 0),
            mp4(
                &[Mp4Item::text(b"\xa9nam", "m"), Mp4Item::cover(14, &tiny_png(3, 3))],
                44_100,
                44_100,
                b"mp4a",
                128_000,
                false,
            ),
            mp4(&[Mp4Item::pair(b"trkn", 1, 2)], 44_100, 44_100, b"alac", 0, true),
            wav(8000, 1, 8, 800, &[("INAM", "w")], Some(&tag)),
        ]
    }

    fn exercise(bytes: &[u8]) {
        let mut src = SliceSource(bytes);
        if let Ok(p) = probe(&mut src)
            && let Some(at) = p.picture
        {
            let _ = load_picture(&mut src, at);
        }
        let _ = load_picture(&mut src, PictureRef::Reparse);
    }

    #[test]
    fn every_prefix_of_every_fixture_is_handled() {
        for f in fixtures() {
            assert!(probe(&mut SliceSource(&f)).is_ok());
            for len in 0..f.len() {
                exercise(&f[..len]);
            }
        }
    }

    #[test]
    fn corrupted_bytes_are_handled() {
        for f in fixtures() {
            // Every byte of the header region, then a stride through the rest.
            let positions = (0..f.len().min(600)).chain((600..f.len()).step_by(7));
            for i in positions {
                for flip in [0xff_u8, 0x80, 0x01] {
                    let mut bad = f.clone();
                    bad[i] ^= flip;
                    exercise(&bad);
                }
            }
        }
    }

    #[test]
    fn arithmetic_helpers() {
        assert_eq!(samples_to_ms(44_100 * 3, 44_100), 3000);
        assert_eq!(samples_to_ms(5, 0), 0);
        assert_eq!(average_kbps(16_000, 1000), 128);
        assert_eq!(average_kbps(1, 0), 0);
    }
}
