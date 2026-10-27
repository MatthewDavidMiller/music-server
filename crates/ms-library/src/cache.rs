//! The index cache: the last scan's track records and folder images, so a
//! restart serves the library at once and the next scan only opens files
//! that changed.
//!
//! ```text
//! "MSIX" | format version u32 | parser version u32 | body | sha256(body)
//! body = record count u32, records..., cover count u32, (dir, image)...
//! ```
//!
//! Integers are little-endian; strings are a u32 length and UTF-8. The file
//! is plain, not sealed: it holds only what the music directory itself
//! exposes, and it is rebuildable, so it lives in the cache directory and
//! is not backed up. A wrong version, a bad checksum, or any structural
//! problem discards it, and the scan that follows reads every file.

use std::path::Path;

use ms_common::buf::Cursor;
use ms_common::{Error, Result, fsio};
use ms_tags::{Codec, Container, Mime, PictureRef};

use crate::model::TrackRecord;

const MAGIC: &[u8; 4] = b"MSIX";
const FORMAT_VERSION: u32 = 1;
pub const MAX_CACHE_BYTES: usize = 512 * 1024 * 1024;
const MAX_STRING: usize = 64 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cached {
    pub records: Vec<TrackRecord>,
    pub folder_covers: Vec<(String, String)>,
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let bytes = &s.as_bytes()[..s.len().min(MAX_STRING)];
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn put_opt(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(s) => {
            out.push(1);
            put_str(out, s);
        }
        None => out.push(0),
    }
}

fn container_code(c: Container) -> u8 {
    match c {
        Container::Mp3 => 0,
        Container::Flac => 1,
        Container::Ogg => 2,
        Container::Mp4 => 3,
        Container::Wav => 4,
    }
}

fn container_from(code: u8) -> Option<Container> {
    Some(match code {
        0 => Container::Mp3,
        1 => Container::Flac,
        2 => Container::Ogg,
        3 => Container::Mp4,
        4 => Container::Wav,
        _ => return None,
    })
}

const CODECS: [Codec; 10] = [
    Codec::Mp3,
    Codec::Mp2,
    Codec::Mp1,
    Codec::Flac,
    Codec::Vorbis,
    Codec::Opus,
    Codec::Aac,
    Codec::Alac,
    Codec::Pcm,
    Codec::Unknown,
];
const MIMES: [Mime; 6] = [Mime::Jpeg, Mime::Png, Mime::Gif, Mime::Bmp, Mime::Webp, Mime::Other];

fn code_of<T: PartialEq>(table: &[T], value: &T) -> u8 {
    u8::try_from(table.iter().position(|v| v == value).unwrap_or(table.len() - 1)).unwrap_or(0)
}

pub fn encode(cached: &Cached) -> Vec<u8> {
    let mut body = Vec::with_capacity(cached.records.len() * 200 + 64);
    body.extend_from_slice(&u32::try_from(cached.records.len()).unwrap_or(0).to_le_bytes());
    for r in &cached.records {
        put_str(&mut body, &r.path);
        body.extend_from_slice(&r.size.to_le_bytes());
        body.extend_from_slice(&r.mtime_ns.to_le_bytes());
        body.extend_from_slice(&r.created.to_le_bytes());
        body.push(container_code(r.container));
        body.push(code_of(&CODECS, &r.codec));
        put_opt(&mut body, r.title.as_deref());
        put_opt(&mut body, r.artist.as_deref());
        put_opt(&mut body, r.album.as_deref());
        put_opt(&mut body, r.album_artist.as_deref());
        body.extend_from_slice(&r.track_no.to_le_bytes());
        body.extend_from_slice(&r.disc_no.to_le_bytes());
        body.extend_from_slice(&r.year.to_le_bytes());
        body.push(u8::try_from(r.genres.len().min(255)).unwrap_or(0));
        for g in r.genres.iter().take(255) {
            put_str(&mut body, g);
        }
        body.push(u8::from(r.compilation));
        body.extend_from_slice(&r.duration_ms.to_le_bytes());
        body.extend_from_slice(&r.bitrate_kbps.to_le_bytes());
        body.extend_from_slice(&r.sample_rate.to_le_bytes());
        body.push(r.channels);
        body.push(r.bit_depth);
        match r.picture {
            None => body.push(0),
            Some(PictureRef::Reparse) => body.push(1),
            Some(PictureRef::Slice { offset, len, mime }) => {
                body.push(2);
                body.extend_from_slice(&offset.to_le_bytes());
                body.extend_from_slice(&len.to_le_bytes());
                body.push(code_of(&MIMES, &mime));
            }
        }
    }
    body.extend_from_slice(&u32::try_from(cached.folder_covers.len()).unwrap_or(0).to_le_bytes());
    for (dir, image) in &cached.folder_covers {
        put_str(&mut body, dir);
        put_str(&mut body, image);
    }
    let mut out = Vec::with_capacity(body.len() + 44);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&ms_tags::PARSER_VERSION.to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&ms_crypto::sha256(&body).unwrap_or([0; 32]));
    out
}

fn get_str(c: &mut Cursor<'_>) -> Option<String> {
    let len = usize::try_from(c.le32()?).ok()?;
    if len > MAX_STRING {
        return None;
    }
    String::from_utf8(c.take(len)?.to_vec()).ok()
}

/// An optional string: `None` for a malformed field, `Some(None)` for an
/// absent value.
#[allow(clippy::option_option)]
fn get_opt(c: &mut Cursor<'_>) -> Option<Option<String>> {
    match c.u8()? {
        0 => Some(None),
        1 => Some(Some(get_str(c)?)),
        _ => None,
    }
}

fn i64_le(c: &mut Cursor<'_>) -> Option<i64> {
    c.le64().map(|v| i64::from_le_bytes(v.to_le_bytes()))
}

fn record(c: &mut Cursor<'_>) -> Option<TrackRecord> {
    let path = get_str(c)?;
    let size = c.le64()?;
    let mtime_ns = i64_le(c)?;
    let created = i64_le(c)?;
    let container = container_from(c.u8()?)?;
    let codec = *CODECS.get(usize::from(c.u8()?))?;
    let title = get_opt(c)?;
    let artist = get_opt(c)?;
    let album = get_opt(c)?;
    let album_artist = get_opt(c)?;
    let track_no = c.le16()?;
    let disc_no = c.le16()?;
    let year = c.le16()?;
    let genre_count = c.u8()?;
    let mut genres = Vec::with_capacity(usize::from(genre_count));
    for _ in 0..genre_count {
        genres.push(get_str(c)?);
    }
    let compilation = c.u8()? != 0;
    let duration_ms = c.le64()?;
    let bitrate_kbps = c.le32()?;
    let sample_rate = c.le32()?;
    let channels = c.u8()?;
    let bit_depth = c.u8()?;
    let picture = match c.u8()? {
        0 => None,
        1 => Some(PictureRef::Reparse),
        2 => {
            let offset = c.le64()?;
            let len = c.le32()?;
            let mime = *MIMES.get(usize::from(c.u8()?))?;
            Some(PictureRef::Slice { offset, len, mime })
        }
        _ => return None,
    };
    Some(TrackRecord {
        path,
        size,
        mtime_ns,
        created,
        container,
        codec,
        title,
        artist,
        album,
        album_artist,
        track_no,
        disc_no,
        year,
        genres,
        compilation,
        duration_ms,
        bitrate_kbps,
        sample_rate,
        channels,
        bit_depth,
        picture,
    })
}

pub fn decode(raw: &[u8]) -> Option<Cached> {
    let mut c = Cursor::new(raw);
    if c.take(4)? != MAGIC || c.le32()? != FORMAT_VERSION || c.le32()? != ms_tags::PARSER_VERSION {
        return None;
    }
    let rest = c.rest();
    let (body, digest) = rest.split_at_checked(rest.len().checked_sub(32)?)?;
    if !ms_crypto::ct_equal(&ms_crypto::sha256(body).ok()?, digest) {
        return None;
    }
    let mut c = Cursor::new(body);
    let count = c.le32()?;
    // Every record takes at least 60 bytes; a count promising more is a lie.
    if u64::from(count) * 60 > body.len() as u64 {
        return None;
    }
    let mut records = Vec::with_capacity(usize::try_from(count).ok()?);
    for _ in 0..count {
        records.push(record(&mut c)?);
    }
    let covers = c.le32()?;
    if u64::from(covers) * 8 > c.remaining() as u64 {
        return None;
    }
    let mut folder_covers = Vec::with_capacity(usize::try_from(covers).ok()?);
    for _ in 0..covers {
        folder_covers.push((get_str(&mut c)?, get_str(&mut c)?));
    }
    (c.remaining() == 0).then_some(Cached { records, folder_covers })
}

/// The cache at `path`, or `None` when it is missing or unusable.
pub fn load(path: &Path) -> Option<Cached> {
    match fsio::read_bounded(path, MAX_CACHE_BYTES) {
        Ok(Some(raw)) => decode(&raw),
        _ => None,
    }
}

pub fn save(path: &Path, cached: &Cached) -> Result<()> {
    let raw = encode(cached);
    if raw.len() > MAX_CACHE_BYTES {
        return Err(Error::new("the library index is too large to cache"));
    }
    fsio::atomic_replace(path, &raw, MAX_CACHE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::tests::rec;

    fn sample() -> Cached {
        let mut a = rec("a/b.mp3", Some("t"), None, Some("al"), Some("aa"), 3);
        a.picture = Some(PictureRef::Slice { offset: 99, len: 12, mime: Mime::Png });
        a.genres = vec!["x".into(), "y".into()];
        let mut b = rec("c.flac", None, Some("ar"), None, None, 0);
        b.picture = Some(PictureRef::Reparse);
        b.container = Container::Flac;
        b.codec = Codec::Flac;
        b.mtime_ns = -5;
        Cached { records: vec![a, b], folder_covers: vec![("a".into(), "a/cover.jpg".into())] }
    }

    #[test]
    fn round_trip() {
        let c = sample();
        assert_eq!(decode(&encode(&c)).unwrap(), c);
        assert_eq!(decode(&encode(&Cached::default())).unwrap(), Cached::default());
    }

    #[test]
    fn damage_is_detected() {
        let raw = encode(&sample());
        for i in 0..raw.len() {
            let mut bad = raw.clone();
            bad[i] ^= 0x40;
            assert!(decode(&bad).is_none(), "byte {i}");
        }
        for len in 0..raw.len() {
            assert!(decode(&raw[..len]).is_none(), "prefix {len}");
        }
    }
}
