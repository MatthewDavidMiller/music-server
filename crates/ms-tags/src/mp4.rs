//! MPEG-4 audio files (ISO/IEC 14496-12 boxes, called atoms here) with
//! iTunes-style metadata: `moov/udta/meta/ilst`.
//!
//! The top level is walked by seeking, so `moov` is found wherever it is,
//! before or after `mdat`; only `moov` itself is read, up to the metadata
//! limit. Inside it: `mvhd` for the duration, the first sound track's
//! `mdhd` and sample description (`mp4a` with its `esds`, or `alac`), and
//! the `ilst` items. Cover art (`covr`) is located inside the `moov` bytes
//! and served from the file by offset.

use crate::limits::{MAX_ITEMS, MAX_METADATA};
use crate::{
    AudioInfo, Codec, Error, Mime, PictureRef, Probe, Result, Source, average_kbps, genre, text,
};
use ms_common::buf::Cursor;

/// Atoms whose children are atoms and which lead to something read here.
const CONTAINERS: &[&[u8; 4]] = &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"udta", b"ilst"];
const MAX_DEPTH: usize = 8;

/// One atom inside a buffer: its type and its body's range.
struct Atom {
    kind: [u8; 4],
    start: usize,
    end: usize,
}

/// The atoms directly inside `buf[start..end]`.
fn children(buf: &[u8], start: usize, end: usize) -> Vec<Atom> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end && out.len() < MAX_ITEMS {
        let mut c = Cursor::new(&buf[pos..end]);
        let Some(size32) = c.be32() else { break };
        let Some(kind) = c.take(4).and_then(|k| <[u8; 4]>::try_from(k).ok()) else { break };
        let (header, size) = match size32 {
            0 => (8, (end - pos) as u64),
            1 => match c.be64() {
                Some(s) => (16, s),
                None => break,
            },
            n => (8, u64::from(n)),
        };
        let Some(size) = usize::try_from(size).ok().filter(|&s| s >= header) else { break };
        let Some(atom_end) = pos.checked_add(size).filter(|&e| e <= end) else { break };
        out.push(Atom { kind, start: pos + header, end: atom_end });
        pos = atom_end;
    }
    out
}

/// The top-level `moov` atom's offset in the file and its bytes.
fn read_moov(src: &mut dyn Source) -> Result<(u64, Vec<u8>)> {
    let size = src.size();
    let mut pos = 0;
    for _ in 0..MAX_ITEMS {
        if pos + 8 > size {
            break;
        }
        let h = src.read_at(pos, 16)?;
        let mut c = Cursor::new(&h);
        let size32 = c.be32().ok_or_else(|| Error::new("truncated atom"))?;
        let kind = c.take(4).ok_or_else(|| Error::new("truncated atom"))?;
        let (header, len) = match size32 {
            0 => (8, size - pos),
            1 => (16, c.be64().ok_or_else(|| Error::new("truncated atom"))?),
            n => (8, u64::from(n)),
        };
        if len < header {
            break;
        }
        if kind == b"moov" {
            let body = usize::try_from(len - header).unwrap_or(usize::MAX);
            if body > MAX_METADATA {
                return Err(Error::new("moov atom exceeds the metadata limit"));
            }
            return Ok((pos + header, src.read_at(pos + header, body)?));
        }
        pos += len;
    }
    Err(Error::new("no moov atom"))
}

/// `mvhd` and `mdhd` share a layout: (timescale, duration).
fn header_times(body: &[u8]) -> Option<(u32, u64)> {
    let mut c = Cursor::new(body);
    let version = c.u8()?;
    c.skip(3)?;
    if version == 1 {
        c.skip(16)?;
        let scale = c.be32()?;
        Some((scale, c.be64()?))
    } else {
        c.skip(8)?;
        let scale = c.be32()?;
        Some((scale, u64::from(c.be32()?)))
    }
}

/// A descriptor's length: up to four bytes of seven bits each.
fn descriptor_len(c: &mut Cursor<'_>) -> Option<usize> {
    let mut len = 0;
    for _ in 0..4 {
        let b = c.u8()?;
        len = len << 7 | usize::from(b & 0x7f);
        if b & 0x80 == 0 {
            return Some(len);
        }
    }
    Some(len)
}

/// The average bitrate (bit/s) in an `esds` atom's decoder configuration.
fn esds_bitrate(body: &[u8]) -> Option<u32> {
    let mut c = Cursor::new(body.get(4..)?);
    if c.u8()? != 0x03 {
        return None;
    }
    descriptor_len(&mut c)?;
    c.skip(2)?; // ES id
    let flags = c.u8()?;
    if flags & 0x80 != 0 {
        c.skip(2)?;
    }
    if flags & 0x40 != 0 {
        let n = c.u8()?;
        c.skip(usize::from(n))?;
    }
    if flags & 0x20 != 0 {
        c.skip(2)?;
    }
    if c.u8()? != 0x04 {
        return None;
    }
    descriptor_len(&mut c)?;
    c.skip(1 + 1 + 3 + 4)?; // object type, stream type, buffer size, max bitrate
    c.be32()
}

struct Track {
    is_sound: bool,
    duration_ms: u64,
    codec: Codec,
    channels: u8,
    sample_rate: u32,
    bit_depth: u8,
    bitrate: u32,
}

fn find<'a>(atoms: &'a [Atom], kind: &[u8]) -> Option<&'a Atom> {
    atoms.iter().find(|a| a.kind[..] == *kind)
}

fn read_track(buf: &[u8], trak: &Atom) -> Option<Track> {
    let mdia = find(&children(buf, trak.start, trak.end), b"mdia").map(|a| (a.start, a.end))?;
    let mdia = children(buf, mdia.0, mdia.1);
    let hdlr = find(&mdia, b"hdlr")?;
    let is_sound = buf.get(hdlr.start + 8..hdlr.start + 12) == Some(b"soun");
    let (scale, duration) = header_times(&buf[find(&mdia, b"mdhd")?.start..])?;
    let mut t = Track {
        is_sound,
        duration_ms: if scale == 0 { 0 } else { duration.saturating_mul(1000) / u64::from(scale) },
        codec: Codec::Unknown,
        channels: 0,
        sample_rate: 0,
        bit_depth: 0,
        bitrate: 0,
    };
    let minf = find(&mdia, b"minf")?;
    let stbl = find(&children(buf, minf.start, minf.end), b"stbl").map(|a| (a.start, a.end))?;
    let stsd = find(&children(buf, stbl.0, stbl.1), b"stsd").map(|a| (a.start, a.end))?;
    // A full atom with an entry count, then the sample entries themselves.
    let entries = children(buf, stsd.0 + 8, stsd.1);
    let entry = entries.first()?;
    t.codec = match &entry.kind {
        b"mp4a" => Codec::Aac,
        b"alac" => Codec::Alac,
        _ => Codec::Unknown,
    };
    // SampleEntry (8) then AudioSampleEntry: version, revision, vendor (8),
    // channels, sample size, compression, packet size, rate (16.16).
    let mut c = Cursor::new(buf.get(entry.start..entry.end)?);
    c.skip(16)?;
    t.channels = u8::try_from(c.be16()?).unwrap_or(0);
    let sample_size = c.be16()?;
    c.skip(4)?;
    t.sample_rate = c.be32()? >> 16;
    let inner = children(buf, entry.start + 28, entry.end);
    match t.codec {
        Codec::Aac => {
            if let Some(esds) = find(&inner, b"esds") {
                t.bitrate = esds_bitrate(&buf[esds.start..esds.end]).unwrap_or(0);
            }
        }
        Codec::Alac => {
            t.bit_depth = u8::try_from(sample_size).unwrap_or(0);
            if let Some(cfg) = find(&inner, b"alac") {
                // version/flags, frame length, compatible version, bit depth
                if let Some(&depth) = buf.get(cfg.start + 9) {
                    t.bit_depth = depth;
                }
            }
        }
        _ => {}
    }
    Some(t)
}

/// The `data` atoms of an `ilst` item: (type indicator, value bytes, offset).
fn item_data(buf: &[u8], item: &Atom) -> Vec<(u32, usize, usize)> {
    children(buf, item.start, item.end)
        .into_iter()
        .filter(|a| &a.kind == b"data" && a.end >= a.start + 8)
        .map(|a| {
            let kind = buf
                .get(a.start..a.start + 4)
                .map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap_or([0; 4])) & 0x00ff_ffff);
            (kind, a.start + 8, a.end)
        })
        .collect()
}

fn apply_ilst(buf: &[u8], moov_offset: u64, ilst: &Atom, probe: &mut Probe) {
    let t = &mut probe.tags;
    for item in children(buf, ilst.start, ilst.end) {
        let data = item_data(buf, &item);
        let Some(&(kind, from, to)) = data.first() else { continue };
        let value = &buf[from..to];
        let string = || text::utf8(value);
        match &item.kind {
            b"\xa9nam" => t.set_named("TITLE", &string()),
            b"\xa9ART" => t.set_named("ARTIST", &string()),
            b"aART" => t.set_named("ALBUMARTIST", &string()),
            b"\xa9alb" => t.set_named("ALBUM", &string()),
            b"\xa9day" => t.set_named("DATE", &string()),
            b"\xa9gen" => t.set_named("GENRE", &string()),
            b"gnre" => {
                if let Some(&[hi, lo]) = value.first_chunk::<2>()
                    && let Some(g) = usize::from(u16::from_be_bytes([hi, lo]))
                        .checked_sub(1)
                        .and_then(genre::name)
                {
                    t.add_genre(g);
                }
            }
            b"trkn" | b"disk" => {
                let mut c = Cursor::new(value);
                let pair = c.skip(2).and_then(|()| Some((c.be16()?, c.be16().unwrap_or(0))));
                if let Some((n, total)) = pair {
                    let n = Some(n).filter(|&v| v > 0);
                    let total = Some(total).filter(|&v| v > 0);
                    if &item.kind == b"trkn" {
                        t.track = t.track.or(n);
                        t.track_total = t.track_total.or(total);
                    } else {
                        t.disc = t.disc.or(n);
                        t.disc_total = t.disc_total.or(total);
                    }
                }
            }
            b"cpil" => t.compilation |= value.first().is_some_and(|&b| b != 0),
            b"covr" => {
                if probe.picture.is_none() {
                    let mime = match kind {
                        13 => Mime::Jpeg,
                        14 => Mime::Png,
                        27 => Mime::Bmp,
                        _ => Mime::Other,
                    };
                    probe.picture = Some(PictureRef::Slice {
                        offset: moov_offset + from as u64,
                        len: u32::try_from(to - from).unwrap_or(0),
                        mime,
                    });
                }
            }
            b"----" => {
                // Freeform: mean, name, data.
                let parts = children(buf, item.start, item.end);
                if let (Some(name), Some(&(_, s, e))) = (find(&parts, b"name"), data.first()) {
                    let key = text::utf8(buf.get(name.start + 4..name.end).unwrap_or_default());
                    t.set_named(&key, &text::utf8(&buf[s..e]));
                }
            }
            _ => {}
        }
    }
}

/// `meta` is a full atom in ISO files and a plain one in some QuickTime
/// writers; the version/flags are present unless `hdlr` follows directly.
fn meta_children(buf: &[u8], meta: &Atom) -> Vec<Atom> {
    let plain = buf.get(meta.start + 4..meta.start + 8) == Some(b"hdlr");
    children(buf, meta.start + if plain { 0 } else { 4 }, meta.end)
}

fn walk_for_ilst(buf: &[u8], start: usize, end: usize, depth: usize) -> Option<Atom> {
    if depth > MAX_DEPTH {
        return None;
    }
    for a in children(buf, start, end) {
        if &a.kind == b"meta" {
            if let Some(ilst) = meta_children(buf, &a).into_iter().find(|c| &c.kind == b"ilst") {
                return Some(ilst);
            }
        } else if CONTAINERS.contains(&&a.kind)
            && let Some(found) = walk_for_ilst(buf, a.start, a.end, depth + 1)
        {
            return Some(found);
        }
    }
    None
}

pub(crate) fn probe(src: &mut dyn Source) -> Result<Probe> {
    let (moov_offset, buf) = read_moov(src)?;
    let mut probe = Probe::default();
    let top = children(&buf, 0, buf.len());
    let movie = find(&top, b"mvhd").and_then(|a| header_times(&buf[a.start..a.end]));
    let track = top
        .iter()
        .filter(|a| &a.kind == b"trak")
        .filter_map(|t| read_track(&buf, t))
        .find(|t| t.is_sound);
    let mut ms =
        movie.map_or(
            0,
            |(scale, d)| if scale == 0 { 0 } else { d.saturating_mul(1000) / u64::from(scale) },
        );
    let mut info = AudioInfo::default();
    if let Some(t) = track {
        if t.duration_ms > 0 {
            ms = t.duration_ms;
        }
        info = AudioInfo {
            duration_ms: 0,
            bitrate_kbps: t.bitrate / 1000,
            sample_rate: t.sample_rate,
            channels: t.channels,
            bit_depth: t.bit_depth,
            codec: t.codec,
        };
    }
    info.duration_ms = ms;
    if info.bitrate_kbps == 0 {
        info.bitrate_kbps = average_kbps(src.size(), ms);
    }
    probe.audio = info;
    if let Some(ilst) = walk_for_ilst(&buf, 0, buf.len(), 0) {
        apply_ilst(&buf, moov_offset, &ilst, &mut probe);
    }
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{Mp4Item, mp4};
    use crate::{SliceSource, load_picture};

    #[test]
    fn itunes_items_and_cover() {
        let items = [
            Mp4Item::text(b"\xa9nam", "AAC Song"),
            Mp4Item::text(b"\xa9ART", "Artist"),
            Mp4Item::text(b"aART", "Album Artist"),
            Mp4Item::text(b"\xa9alb", "Album"),
            Mp4Item::text(b"\xa9day", "2015-06-01T00:00:00Z"),
            Mp4Item::pair(b"trkn", 5, 9),
            Mp4Item::pair(b"disk", 1, 1),
            Mp4Item::genre(18),
            Mp4Item::flag(b"cpil", true),
            Mp4Item::cover(13, b"\xff\xd8cover"),
        ];
        for moov_last in [false, true] {
            let file = mp4(&items, 44_100, 44_100 * 7, b"mp4a", 256_000, moov_last);
            let mut src = SliceSource(&file);
            let p = crate::probe(&mut src).unwrap();
            let t = &p.tags;
            assert_eq!(t.title.as_deref(), Some("AAC Song"));
            assert_eq!(t.album_artist.as_deref(), Some("Album Artist"));
            assert_eq!(t.year, Some(2015));
            assert_eq!((t.track, t.track_total, t.disc), (Some(5), Some(9), Some(1)));
            assert_eq!(t.genres, ["Rock"]);
            assert!(t.compilation);
            assert_eq!(p.audio.duration_ms, 7000);
            assert_eq!(p.audio.codec, Codec::Aac);
            assert_eq!(
                (p.audio.sample_rate, p.audio.channels, p.audio.bitrate_kbps),
                (44_100, 2, 256)
            );
            let pic = load_picture(&mut src, p.picture.unwrap()).unwrap().unwrap();
            assert_eq!(pic.data, b"\xff\xd8cover");
        }
    }

    #[test]
    fn alac_and_missing_moov() {
        let file = mp4(&[], 48_000, 48_000, b"alac", 0, false);
        let p = crate::probe(&mut SliceSource(&file)).unwrap();
        assert_eq!(p.audio.codec, Codec::Alac);
        assert_eq!(p.audio.bit_depth, 16);
        assert!(probe(&mut SliceSource(b"\0\0\0\x10ftypM4A \0\0\0\0")).is_err());
    }
}
