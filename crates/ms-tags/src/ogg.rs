//! Ogg Vorbis and Ogg Opus (RFC 3533 pages, RFC 7845 for Opus).
//!
//! The first two packets of the first logical stream are reassembled from
//! its pages: the identification header (codec, channels, rate) and the
//! comment header. The duration comes from the granule position of the
//! stream's last page, found by searching backwards from the end of the
//! file. Page checksums are not verified; a damaged page only makes the
//! result less complete.

use crate::limits::{MAX_ITEMS, MAX_METADATA};
use crate::{
    AudioInfo, Codec, Error, Picture, PictureRef, Probe, Result, Source, average_kbps,
    samples_to_ms, vorbis,
};

/// How much of the file's end is searched for the last page.
const TAIL_SEARCH: [usize; 2] = [64 * 1024, 1024 * 1024];

struct Page {
    serial: u32,
    /// Lacing values: each segment's length.
    segments: Vec<u8>,
    data_start: u64,
    end: u64,
}

fn page_at(src: &mut dyn Source, pos: u64) -> Result<Option<Page>> {
    let h = src.read_at(pos, 27)?;
    if h.len() < 27 || &h[..4] != b"OggS" || h[4] != 0 {
        return Ok(None);
    }
    let serial = u32::from_le_bytes(h[14..18].try_into().unwrap_or([0; 4]));
    let count = usize::from(h[26]);
    let segments = src.read_at(pos + 27, count)?;
    if segments.len() < count {
        return Ok(None);
    }
    let body: u64 = segments.iter().map(|&s| u64::from(s)).sum();
    let data_start = pos + 27 + count as u64;
    Ok(Some(Page { serial, segments, data_start, end: data_start + body }))
}

/// The first `wanted` packets of the stream that starts the file, each
/// capped at [`MAX_METADATA`] bytes (a longer one is returned truncated).
fn first_packets(src: &mut dyn Source, wanted: usize) -> Result<(u32, Vec<Vec<u8>>, u64)> {
    let mut packets: Vec<Vec<u8>> = vec![Vec::new()];
    let mut pos = 0;
    let mut serial = None;
    for _ in 0..MAX_ITEMS {
        let Some(page) = page_at(src, pos)? else { break };
        pos = page.end;
        if *serial.get_or_insert(page.serial) != page.serial {
            continue; // another multiplexed stream
        }
        // At most 255 segments of 255 bytes: one read covers the page.
        let body_len = usize::try_from(page.end - page.data_start).unwrap_or(0);
        let body = src.read_at(page.data_start, body_len)?;
        let mut at = 0;
        for &seg in &page.segments {
            let current = packets.last_mut().ok_or_else(|| Error::new("no packet"))?;
            let piece = body.get(at..at + usize::from(seg)).unwrap_or_default();
            if current.len() < MAX_METADATA {
                let take = piece.len().min(MAX_METADATA - current.len());
                current.extend_from_slice(&piece[..take]);
            }
            at += usize::from(seg);
            if seg < 255 {
                if packets.len() == wanted {
                    return Ok((page.serial, packets, pos));
                }
                packets.push(Vec::new());
            }
        }
    }
    packets.retain(|p| !p.is_empty());
    Ok((serial.unwrap_or(0), packets, pos))
}

/// The granule position of `serial`'s last page.
fn last_granule(src: &mut dyn Source, serial: u32) -> Result<Option<u64>> {
    let size = src.size();
    for window in TAIL_SEARCH {
        let start = size.saturating_sub(window as u64);
        let buf = src.read_at(start, window)?;
        let mut i = buf.len().saturating_sub(27);
        loop {
            if buf[i..].starts_with(b"OggS")
                && let Some(h) = buf.get(i..i + 27)
                && u32::from_le_bytes(h[14..18].try_into().unwrap_or([0; 4])) == serial
            {
                let g = u64::from_le_bytes(h[6..14].try_into().unwrap_or([0; 8]));
                if g != u64::MAX {
                    return Ok(Some(g));
                }
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
        if start == 0 {
            break;
        }
    }
    Ok(None)
}

struct Walk {
    probe: Probe,
    picture: Option<Picture>,
}

fn walk(src: &mut dyn Source, want_picture: bool) -> Result<Walk> {
    let (serial, packets, _) = first_packets(src, 2)?;
    let id = packets.first().ok_or_else(|| Error::new("empty Ogg stream"))?;
    let mut w = Walk { probe: Probe::default(), picture: None };
    let (codec, comment_prefix, pre_skip): (Codec, &[u8], u64) = if id.starts_with(b"\x01vorbis") {
        let mut c = ms_common::buf::Cursor::new(&id[7..]);
        c.skip(4).ok_or_else(|| Error::new("truncated Vorbis header"))?;
        w.probe.audio.channels = c.u8().unwrap_or(0);
        w.probe.audio.sample_rate = c.le32().unwrap_or(0);
        (Codec::Vorbis, b"\x03vorbis", 0)
    } else if id.starts_with(b"OpusHead") {
        let mut c = ms_common::buf::Cursor::new(&id[8..]);
        c.skip(1).ok_or_else(|| Error::new("truncated Opus header"))?;
        w.probe.audio.channels = c.u8().unwrap_or(0);
        let pre_skip = c.le16().unwrap_or(0);
        // Opus always decodes at 48 kHz; the input rate is informational.
        w.probe.audio.sample_rate = 48_000;
        (Codec::Opus, b"OpusTags", u64::from(pre_skip))
    } else {
        return Err(Error::new("unsupported Ogg codec"));
    };
    w.probe.audio.codec = codec;
    if let Some(comments) = packets.get(1).and_then(|p| p.strip_prefix(comment_prefix)) {
        let c = vorbis::parse(comments, want_picture);
        w.probe.tags = c.tags;
        if c.has_picture {
            w.probe.picture = Some(PictureRef::Reparse);
        }
        w.picture = c.picture;
    }
    if !want_picture {
        let rate = w.probe.audio.sample_rate;
        let samples = last_granule(src, serial)?.unwrap_or(0).saturating_sub(pre_skip);
        let ms = samples_to_ms(samples, rate);
        w.probe.audio = AudioInfo {
            duration_ms: ms,
            bitrate_kbps: average_kbps(src.size(), ms),
            ..w.probe.audio
        };
    }
    Ok(w)
}

pub(crate) fn probe(src: &mut dyn Source) -> Result<Probe> {
    walk(src, false).map(|w| w.probe)
}

pub(crate) fn picture(src: &mut dyn Source) -> Result<Option<Picture>> {
    walk(src, true).map(|w| w.picture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{flac_picture, ogg_stream, vorbis_comments};
    use crate::{SliceSource, load_picture};

    #[test]
    fn opus_stream() {
        let pic = flac_picture(3, "image/png", b"PNG!");
        let comments = vorbis_comments(&[
            "TITLE=Opus Song".into(),
            format!("METADATA_BLOCK_PICTURE={}", ms_common::base64::encode(&pic)),
        ]);
        // 5 s plus a 312-sample pre-skip, at 48 kHz; the comment packet is
        // padded past one page to exercise packet reassembly.
        let file = ogg_stream(true, &comments, 240_000 + 312, 70_000);
        let mut src = SliceSource(&file);
        let p = probe(&mut src).unwrap();
        assert_eq!(p.audio.codec, Codec::Opus);
        assert_eq!(p.tags.title.as_deref(), Some("Opus Song"));
        assert_eq!(p.audio.duration_ms, 5000);
        assert_eq!(p.audio.channels, 2);
        assert_eq!(p.picture, Some(PictureRef::Reparse));
        assert_eq!(load_picture(&mut src, PictureRef::Reparse).unwrap().unwrap().data, b"PNG!");
    }

    #[test]
    fn vorbis_stream() {
        let comments = vorbis_comments(&["ARTIST=Vorbis Band".into()]);
        let file = ogg_stream(false, &comments, 44_100 * 3, 0);
        let p = probe(&mut SliceSource(&file)).unwrap();
        assert_eq!(p.audio.codec, Codec::Vorbis);
        assert_eq!(p.audio.sample_rate, 44_100);
        assert_eq!(p.audio.duration_ms, 3000);
        assert_eq!(p.tags.artist.as_deref(), Some("Vorbis Band"));
        assert_eq!(p.picture, None);
    }
}
