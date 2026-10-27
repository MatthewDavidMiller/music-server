//! ID3v2.2, 2.3, and 2.4 (id3.org's informal standards).
//!
//! The tag is walked frame by frame through [`Source`] reads, so a picture
//! frame of several megabytes costs one small read of its header: its data
//! is recorded by position and skipped. A tag whose bytes are
//! unsynchronised as a whole (ID3v2.3's global flag) is the exception: frame
//! boundaries can only be found after undoing the transformation, so that
//! tag is read whole (up to the metadata limit) and walked in memory, and
//! its picture is marked for a re-parse.
//!
//! Compressed and encrypted frames are skipped.

use crate::limits::{MAX_FIELD, MAX_ITEMS, MAX_METADATA, MAX_PICTURE};
use crate::{Container, Mime, Picture, PictureRef, Result, SliceSource, Source, Tags, genre, text};

/// What is read of a picture frame to find where its image starts: the
/// encoding, MIME type, picture type, and description.
const PICTURE_HEADER_READ: usize = 64 * 1024;

#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub(crate) tags: Tags,
    pub(crate) picture: Option<PictureRef>,
    /// The decoded picture, when it was asked for.
    pub(crate) picture_data: Option<Picture>,
    /// The offset just past the tag.
    pub(crate) end: u64,
}

struct Header {
    major: u8,
    flags: u8,
    end: u64,
}

fn header(src: &mut dyn Source, offset: u64) -> Result<Option<Header>> {
    let h = src.read_at(offset, 10)?;
    if h.len() < 10 || &h[..3] != b"ID3" || !(2..=4).contains(&h[3]) || h[4] == 0xff {
        return Ok(None);
    }
    let Some(size) = ms_common::buf::Cursor::new(&h[6..10]).syncsafe32() else {
        return Ok(None);
    };
    let footer = if h[3] == 4 && h[5] & 0x10 != 0 { 10 } else { 0 };
    Ok(Some(Header { major: h[3], flags: h[5], end: offset + 10 + u64::from(size) + footer }))
}

/// The offset just past any ID3v2 tags at `offset` (some files carry more
/// than one), or `offset` itself when there is none.
pub(crate) fn skip_tags(src: &mut dyn Source, mut offset: u64) -> Result<u64> {
    for _ in 0..4 {
        match header(src, offset)? {
            Some(h) => offset = h.end,
            None => break,
        }
    }
    Ok(offset)
}

/// The container that follows a leading ID3v2 tag.
pub(crate) fn container_after_tag(src: &mut dyn Source) -> Option<Container> {
    let after = skip_tags(src, 0).ok()?;
    let head = src.read_at(after, 4).ok()?;
    if head.starts_with(b"fLaC") {
        Some(Container::Flac)
    } else {
        // MPEG audio, possibly after padding: the frame scanner decides.
        Some(Container::Mp3)
    }
}

/// Reverses unsynchronisation: every `FF 00` becomes `FF`.
fn resync(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut prev_ff = false;
    for &b in data {
        if prev_ff && b == 0 {
            prev_ff = false;
            continue;
        }
        out.push(b);
        prev_ff = b == 0xff;
    }
    out
}

/// Reads the tag at `offset`. `Ok(None)` when there is no tag there.
pub(crate) fn read(
    src: &mut dyn Source,
    offset: u64,
    want_picture: bool,
) -> Result<Option<Parsed>> {
    let Some(h) = header(src, offset)? else { return Ok(None) };
    let mut parsed = Parsed { end: h.end, ..Parsed::default() };
    // ID3v2.2 compression was never defined; such a tag cannot be read.
    if h.major == 2 && h.flags & 0x40 != 0 {
        return Ok(Some(parsed));
    }
    let body_start = offset + 10;
    let body_end = h.end.min(src.size());
    if h.flags & 0x80 != 0 && h.major <= 3 {
        let len = usize::try_from(body_end.saturating_sub(body_start)).unwrap_or(usize::MAX);
        let raw = src.read_at(body_start, len.min(MAX_METADATA))?;
        let body = resync(&raw);
        let mut mem = SliceSource(&body);
        let size = body.len() as u64;
        walk(&mut mem, 0, size, h.major, h.flags, true, want_picture, &mut parsed)?;
    } else {
        walk(src, body_start, body_end, h.major, h.flags, false, want_picture, &mut parsed)?;
    }
    Ok(Some(parsed))
}

pub(crate) fn picture(src: &mut dyn Source, offset: u64) -> Result<Option<Picture>> {
    Ok(read(src, offset, true)?.and_then(|p| p.picture_data))
}

/// The ID3v2.3 name of an ID3v2.2 frame this reader understands.
fn v22_name(id: &[u8]) -> Option<&'static str> {
    Some(match id {
        b"TT2" => "TIT2",
        b"TP1" => "TPE1",
        b"TP2" => "TPE2",
        b"TAL" => "TALB",
        b"TRK" => "TRCK",
        b"TPA" => "TPOS",
        b"TYE" => "TYER",
        b"TOR" => "TORY",
        b"TCO" => "TCON",
        b"TCP" => "TCMP",
        b"TXX" => "TXXX",
        b"PIC" => "PIC",
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn walk(
    src: &mut dyn Source,
    start: u64,
    end: u64,
    major: u8,
    tag_flags: u8,
    in_memory: bool,
    want_picture: bool,
    out: &mut Parsed,
) -> Result<()> {
    let (id_len, header_len) = if major == 2 { (3, 6) } else { (4, 10) };
    let mut pos = start;
    // The extended header, which this reader has no use for.
    if major >= 3 && tag_flags & 0x40 != 0 {
        let raw = src.read_at(pos, 4)?;
        let mut c = ms_common::buf::Cursor::new(&raw);
        let skip = if major == 3 {
            c.be32().map(|n| u64::from(n) + 4)
        } else {
            c.syncsafe32().map(u64::from)
        };
        match skip {
            Some(n) => pos += n,
            None => return Ok(()),
        }
    }
    let global_unsync = major == 4 && tag_flags & 0x80 != 0;
    let mut best_picture_type: Option<u8> = None;
    for _ in 0..MAX_ITEMS {
        if pos + header_len > end {
            break;
        }
        let h = src.read_at(pos, usize::try_from(header_len).unwrap_or(10))?;
        if h.len() < usize::try_from(header_len).unwrap_or(10) {
            break;
        }
        let id = &h[..id_len];
        if id[0] == 0 || !id.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
            break; // padding, or garbage
        }
        let mut c = ms_common::buf::Cursor::new(&h[id_len..]);
        let size = match major {
            2 => c.be24(),
            3 => c.be32(),
            // Some writers stored plain sizes in 2.4 tags; take those as-is.
            _ => {
                let mut probe = c;
                probe.syncsafe32().or_else(|| c.be32())
            }
        };
        let Some(size) = size else { break };
        let data_start = pos + header_len;
        let next = data_start + u64::from(size);
        if next > end {
            break;
        }
        pos = next;
        let format = if major >= 3 { h[9] } else { 0 };
        let mut extra: u64 = 0;
        let mut unsync = global_unsync;
        match major {
            3 => {
                if format & 0xc0 != 0 {
                    continue; // compressed or encrypted
                }
                if format & 0x20 != 0 {
                    extra += 1;
                }
            }
            4 => {
                if format & 0x0c != 0 {
                    continue;
                }
                if format & 0x40 != 0 {
                    extra += 1;
                }
                if format & 0x01 != 0 {
                    extra += 4;
                }
                unsync |= format & 0x02 != 0;
            }
            _ => {}
        }
        let Some(len) = u64::from(size).checked_sub(extra) else { continue };
        let data_start = data_start + extra;
        let name: &str = if major == 2 {
            match v22_name(id) {
                Some(n) => n,
                None => continue,
            }
        } else {
            match std::str::from_utf8(id) {
                Ok(n) => n,
                Err(_) => continue,
            }
        };
        if name == "APIC" || name == "PIC" {
            let len = usize::try_from(len).unwrap_or(usize::MAX);
            let head = src.read_at(data_start, len.min(PICTURE_HEADER_READ))?;
            let head = if unsync { resync(&head) } else { head };
            let Some((ptype, mime, image_at)) = picture_header(&head, name == "PIC") else {
                continue;
            };
            let better = match best_picture_type {
                None => true,
                Some(t) => t != 3 && ptype == 3,
            };
            if !better {
                continue;
            }
            best_picture_type = Some(ptype);
            let transformed = unsync || in_memory;
            out.picture = Some(if transformed {
                PictureRef::Reparse
            } else {
                PictureRef::Slice {
                    offset: data_start + image_at as u64,
                    len: u32::try_from(len.saturating_sub(image_at)).unwrap_or(0),
                    mime,
                }
            });
            if want_picture {
                if len > MAX_PICTURE {
                    continue;
                }
                let raw = src.read_at(data_start, len)?;
                let raw = if unsync { resync(&raw) } else { raw };
                if let Some(data) = raw.get(image_at..) {
                    out.picture_data = Some(Picture { data: data.to_vec(), mime });
                }
            }
            continue;
        }
        if !name.starts_with('T') || len > MAX_FIELD as u64 {
            continue;
        }
        let raw = src.read_at(data_start, usize::try_from(len).unwrap_or(0))?;
        let raw = if unsync { resync(&raw) } else { raw };
        apply_text_frame(&mut out.tags, name, &decode_text(&raw));
    }
    Ok(())
}

/// The values of a text frame: an encoding byte, then strings separated by
/// the encoding's terminator (ID3v2.4 multi-value frames).
pub(crate) fn decode_text(data: &[u8]) -> Vec<String> {
    let Some((&enc, body)) = data.split_first() else { return Vec::new() };
    split_terminated(enc, body).into_iter().map(|piece| decode(enc, piece)).collect()
}

fn decode(enc: u8, bytes: &[u8]) -> String {
    match enc {
        0 => text::latin1(bytes),
        1 => text::utf16(bytes, false),
        2 => text::utf16(bytes, true),
        _ => text::utf8(bytes),
    }
}

/// Splits on NUL (one byte for Latin-1 and UTF-8, an aligned pair for
/// UTF-16), dropping a trailing empty piece.
fn split_terminated(enc: u8, body: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let wide = enc == 1 || enc == 2;
    let step = if wide { 2 } else { 1 };
    let mut start = 0;
    let mut i = 0;
    while i + step <= body.len() {
        if body[i..i + step].iter().all(|&b| b == 0) {
            out.push(&body[start..i]);
            start = i + step;
        }
        i += step;
    }
    if start < body.len() {
        out.push(&body[start..]);
    }
    out
}

/// The length of a terminated string at the start of `data` in `enc`,
/// including its terminator, or `None` when it is unterminated.
fn terminated_len(enc: u8, data: &[u8]) -> Option<usize> {
    if enc == 1 || enc == 2 {
        data.as_chunks::<2>().0.iter().position(|p| p == &[0, 0]).map(|i| i * 2 + 2)
    } else {
        data.iter().position(|&b| b == 0).map(|i| i + 1)
    }
}

/// Parses the fixed part of an APIC (or v2.2 PIC) frame: the picture type,
/// its MIME type, and the offset of the image data within the frame.
fn picture_header(d: &[u8], v22: bool) -> Option<(u8, Mime, usize)> {
    let (&enc, rest) = d.split_first()?;
    let (mime, after_mime) = if v22 {
        let fmt = rest.get(..3)?;
        (Mime::from_name(&text::latin1(fmt)), 4)
    } else {
        let n = rest.iter().position(|&b| b == 0)?;
        (Mime::from_name(&text::latin1(&rest[..n])), 1 + n + 1)
    };
    let ptype = *d.get(after_mime)?;
    let desc_start = after_mime + 1;
    let desc_len = terminated_len(enc, d.get(desc_start..)?)?;
    Some((ptype, mime, desc_start + desc_len))
}

fn apply_text_frame(tags: &mut Tags, name: &str, values: &[String]) {
    let first = values.first().map_or("", String::as_str);
    match name {
        "TIT2" => tags.set_named("TITLE", first),
        "TPE1" => tags.set_named("ARTIST", &values.join("; ")),
        "TPE2" => tags.set_named("ALBUMARTIST", first),
        "TALB" => tags.set_named("ALBUM", first),
        "TRCK" => tags.set_named("TRACKNUMBER", first),
        "TPOS" => tags.set_named("DISCNUMBER", first),
        "TYER" | "TDRC" | "TORY" | "TDOR" => tags.set_named("DATE", first),
        "TCMP" => tags.set_named("COMPILATION", first),
        "TCON" => {
            for v in values {
                for g in content_types(v) {
                    tags.add_genre(&g);
                }
            }
        }
        "TXXX" => {
            if let (Some(desc), Some(value)) = (values.first(), values.get(1)) {
                match desc.to_ascii_uppercase().as_str() {
                    "ALBUMARTIST" | "ALBUM ARTIST" | "ALBUM_ARTIST" => {
                        tags.set_named("ALBUMARTIST", value);
                    }
                    "TOTALTRACKS" | "TRACKTOTAL" => tags.set_named("TRACKTOTAL", value),
                    "TOTALDISCS" | "DISCTOTAL" => tags.set_named("DISCTOTAL", value),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// A TCON value: free text, an ID3v1 index ("17"), references ("(17)"),
/// or both ("(17)Rock"); "(RX)" and "(CR)" are Remix and Cover.
fn content_types(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut s = value.trim();
    while let Some(rest) = s.strip_prefix('(') {
        if rest.starts_with('(') {
            break; // "((" escapes a literal parenthesis
        }
        let Some(close) = rest.find(')') else { break };
        let inner = &rest[..close];
        match inner {
            "RX" => out.push("Remix".to_owned()),
            "CR" => out.push("Cover".to_owned()),
            n => {
                if let Some(g) = n.parse::<usize>().ok().and_then(genre::name) {
                    out.push(g.to_owned());
                }
            }
        }
        s = rest[close + 1..].trim_start();
    }
    if !s.is_empty() {
        if let Ok(n) = s.parse::<usize>() {
            out.extend(genre::name(n).map(str::to_owned));
        } else {
            let literal = s.strip_prefix('(').unwrap_or(s);
            out.retain(|g| !g.eq_ignore_ascii_case(literal));
            out.push(literal.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{Id3Frame, id3v2};

    fn parse(bytes: &[u8]) -> Parsed {
        read(&mut SliceSource(bytes), 0, true).unwrap().unwrap()
    }

    #[test]
    fn genres() {
        assert_eq!(content_types("(17)"), ["Rock"]);
        assert_eq!(content_types("(17)Rock"), ["Rock"]);
        assert_eq!(content_types("(13)(17)Pop Punk"), ["Pop", "Rock", "Pop Punk"]);
        assert_eq!(content_types("17"), ["Rock"]);
        assert_eq!(content_types("Shoegaze"), ["Shoegaze"]);
        assert_eq!(content_types("(RX)"), ["Remix"]);
        assert_eq!(content_types("(999)"), Vec::<String>::new());
    }

    #[test]
    fn text_values() {
        assert_eq!(decode_text(b"\x00caf\xe9\x00b"), ["café", "b"]);
        assert_eq!(decode_text(b"\x03a\x00b\x00"), ["a", "b"]);
        assert_eq!(decode_text(&[1, 0xff, 0xfe, b'x', 0, 0, 0, 0xff, 0xfe, b'y', 0]), ["x", "y"]);
        assert_eq!(decode_text(&[2, 0, b'z']), ["z"]);
        assert!(decode_text(b"").is_empty());
    }

    #[test]
    fn each_version_reads_its_frames() {
        for major in [2_u8, 3, 4] {
            let tag = id3v2(
                major,
                &[
                    Id3Frame::text("TIT2", "Title"),
                    Id3Frame::text("TPE1", "Artist"),
                    Id3Frame::text("TPE2", "Album Artist"),
                    Id3Frame::text("TALB", "Album"),
                    Id3Frame::text("TRCK", "4/10"),
                    Id3Frame::text("TPOS", "1/2"),
                    Id3Frame::text(if major == 4 { "TDRC" } else { "TYER" }, "2004"),
                    Id3Frame::text("TCON", "(17)"),
                    Id3Frame::picture(3, "image/png", b"\x89PNG-bytes"),
                ],
                false,
            );
            let p = parse(&tag);
            let t = &p.tags;
            assert_eq!(t.title.as_deref(), Some("Title"), "v2.{major}");
            assert_eq!(t.artist.as_deref(), Some("Artist"));
            assert_eq!(t.album_artist.as_deref(), Some("Album Artist"));
            assert_eq!(t.album.as_deref(), Some("Album"));
            assert_eq!(
                (t.track, t.track_total, t.disc, t.disc_total),
                (Some(4), Some(10), Some(1), Some(2))
            );
            assert_eq!(t.year, Some(2004));
            assert_eq!(t.genres, ["Rock"]);
            assert_eq!(p.end, tag.len() as u64);
            let Some(PictureRef::Slice { offset, len, .. }) = p.picture else {
                panic!("v2.{major}: {:?}", p.picture)
            };
            let start = usize::try_from(offset).unwrap();
            assert_eq!(&tag[start..start + len as usize], b"\x89PNG-bytes");
            assert_eq!(p.picture_data.unwrap().data, b"\x89PNG-bytes");
        }
    }

    #[test]
    fn front_cover_wins_over_other_pictures() {
        let tag = id3v2(
            3,
            &[
                Id3Frame::picture(4, "image/jpeg", b"back"),
                Id3Frame::picture(3, "image/jpeg", b"front"),
            ],
            false,
        );
        assert_eq!(parse(&tag).picture_data.unwrap().data, b"front");
    }

    #[test]
    fn unsynchronised_tags_resync_and_mark_pictures_for_reparse() {
        let image = [0xff, 0x00, 0xff, 0xe0, 0x12];
        let tag = id3v2(
            3,
            &[Id3Frame::text("TIT2", "Sync"), Id3Frame::picture(3, "image/jpeg", &image)],
            true,
        );
        let p = parse(&tag);
        assert_eq!(p.tags.title.as_deref(), Some("Sync"));
        assert_eq!(p.picture, Some(PictureRef::Reparse));
        assert_eq!(p.picture_data.unwrap().data, image);
    }

    #[test]
    fn txxx_album_artist_and_utf16() {
        let mut f = Id3Frame::raw("TXXX", b"\x03ALBUM ARTIST\x00Somebody".to_vec());
        let tag = id3v2(3, &[f.clone()], false);
        assert_eq!(parse(&tag).tags.album_artist.as_deref(), Some("Somebody"));
        f = Id3Frame::raw("TIT2", vec![1, 0xff, 0xfe, 0xe9, 0, b't', 0]);
        assert_eq!(parse(&id3v2(3, &[f], false)).tags.title.as_deref(), Some("ét"));
    }

    #[test]
    fn no_tag_and_garbage() {
        assert!(read(&mut SliceSource(b"not a tag at all"), 0, false).unwrap().is_none());
        // A tag claiming more bytes than exist stops at the end quietly.
        let mut tag = id3v2(3, &[Id3Frame::text("TIT2", "x")], false);
        tag.truncate(15);
        assert!(parse(&tag).tags.title.is_none());
    }
}
