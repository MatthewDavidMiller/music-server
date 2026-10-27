//! MPEG-1/2/2.5 audio (layers I-III): the stream that follows the ID3 tags.
//!
//! The first frame is found by scanning for a header that is followed by two
//! more valid headers at the offsets its length predicts, which rejects the
//! false syncs that cover art and padding are full of. Its Xing/Info or VBRI
//! header, when present, gives the exact frame count of a VBR file;
//! otherwise the first frame's bitrate is taken as constant.

use crate::{
    AudioInfo, Codec, Error, Probe, Result, Source, average_kbps, id3v1, id3v2, samples_to_ms,
};

/// How far past the tags the first frame is looked for.
const SYNC_SEARCH: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    /// 1 for MPEG-1, 2 for MPEG-2, 25 for MPEG-2.5.
    version: u8,
    layer: u8,
    bitrate_kbps: u32,
    sample_rate: u32,
    channels: u8,
    pub(crate) length: usize,
    samples: u32,
}

const BITRATES_V1: [[u32; 15]; 3] = [
    [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
    [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
];
const BITRATES_V2: [[u32; 15]; 3] = [
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

pub(crate) fn parse_header(b: &[u8]) -> Option<FrameHeader> {
    let [b0, b1, b2, b3] = *b.first_chunk::<4>()?;
    if b0 != 0xff || b1 & 0xe0 != 0xe0 {
        return None;
    }
    let version = match (b1 >> 3) & 3 {
        0 => 25,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let layer = match (b1 >> 1) & 3 {
        1 => 3,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let bitrate_index = usize::from(b2 >> 4);
    let rate_index = (b2 >> 2) & 3;
    if bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None; // free format is not supported
    }
    let table = if version == 1 { &BITRATES_V1 } else { &BITRATES_V2 };
    let bitrate_kbps = table[usize::from(layer - 1)][bitrate_index];
    let base = [44_100, 48_000, 32_000][usize::from(rate_index)];
    let sample_rate = match version {
        1 => base,
        2 => base / 2,
        _ => base / 4,
    };
    let padding = u32::from((b2 >> 1) & 1);
    let channels = if b3 >> 6 == 3 { 1 } else { 2 };
    let (samples, length) = match layer {
        1 => (384, (12 * bitrate_kbps * 1000 / sample_rate + padding) * 4),
        2 => (1152, 144 * bitrate_kbps * 1000 / sample_rate + padding),
        _ if version == 1 => (1152, 144 * bitrate_kbps * 1000 / sample_rate + padding),
        _ => (576, 72 * bitrate_kbps * 1000 / sample_rate + padding),
    };
    Some(FrameHeader {
        version,
        layer,
        bitrate_kbps,
        sample_rate,
        channels,
        length: usize::try_from(length).ok()?,
        samples,
    })
}

/// The first frame at or after the start of `buf` whose two successors
/// agree with it; the end of the buffer counts as agreement.
fn find_first_frame(buf: &[u8]) -> Option<(usize, FrameHeader)> {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if buf[i] == 0xff
            && let Some(h) = parse_header(&buf[i..])
            && h.length >= 4
        {
            let mut at = i + h.length;
            let mut ok = true;
            for _ in 0..2 {
                if at + 4 > buf.len() {
                    break;
                }
                match parse_header(&buf[at..]) {
                    Some(n)
                        if n.version == h.version
                            && n.layer == h.layer
                            && n.sample_rate == h.sample_rate =>
                    {
                        at += n.length.max(1);
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                return Some((i, h));
            }
        }
        i += 1;
    }
    None
}

/// Frame and byte counts from a Xing/Info or VBRI header in `frame`.
fn vbr_counts(frame: &[u8], h: &FrameHeader) -> Option<(u32, Option<u32>)> {
    let side = match (h.version, h.channels) {
        (1, 1) => 17,
        (1, _) => 32,
        (_, 1) => 9,
        _ => 17,
    };
    if let Some(x) = frame.get(4 + side..)
        && (x.starts_with(b"Xing") || x.starts_with(b"Info"))
    {
        let mut c = ms_common::buf::Cursor::new(&x[4..]);
        let flags = c.be32()?;
        let frames = if flags & 1 != 0 { c.be32() } else { None };
        let bytes = if flags & 2 != 0 { c.be32() } else { None };
        return frames.map(|f| (f, bytes));
    }
    if let Some(v) = frame.get(36..)
        && v.starts_with(b"VBRI")
    {
        let mut c = ms_common::buf::Cursor::new(&v[4..]);
        c.skip(6)?; // version, delay, quality
        let bytes = c.be32()?;
        let frames = c.be32()?;
        return Some((frames, Some(bytes)));
    }
    None
}

pub(crate) fn probe(src: &mut dyn Source) -> Result<Probe> {
    let mut probe = Probe::default();
    let id3 = id3v2::read(src, 0, false)?;
    let audio_start = match &id3 {
        Some(p) => id3v2::skip_tags(src, p.end)?,
        None => 0,
    };
    if let Some(p) = id3 {
        probe.tags = p.tags;
        probe.picture = p.picture;
    }
    let tail = id3v1::read(src)?;
    let audio_end = src.size() - if tail.is_some() { id3v1::LEN } else { 0 };
    if let Some(t) = tail {
        probe.tags.fill_from(t);
    }
    let buf = src.read_at(audio_start, SYNC_SEARCH)?;
    let (at, h) = find_first_frame(&buf).ok_or_else(|| Error::new("no MPEG audio frames found"))?;
    let first_frame = audio_start + at as u64;
    let audio_bytes = audio_end.saturating_sub(first_frame);
    let frame = &buf[at..buf.len().min(at + h.length.max(4))];
    let (duration_ms, bitrate_kbps) = match vbr_counts(frame, &h) {
        Some((frames, bytes)) if frames > 0 => {
            let ms = samples_to_ms(u64::from(frames) * u64::from(h.samples), h.sample_rate);
            let bytes = bytes.map_or(audio_bytes, u64::from);
            (ms, average_kbps(bytes, ms))
        }
        _ => {
            let ms = audio_bytes.saturating_mul(8) / u64::from(h.bitrate_kbps.max(1));
            (ms, h.bitrate_kbps)
        }
    };
    probe.audio = AudioInfo {
        duration_ms,
        bitrate_kbps,
        sample_rate: h.sample_rate,
        channels: h.channels,
        bit_depth: 0,
        codec: match h.layer {
            1 => Codec::Mp1,
            2 => Codec::Mp2,
            _ => Codec::Mp3,
        },
    };
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SliceSource;
    use crate::testgen::{Id3Frame, id3v2, mpeg_frames};

    #[test]
    fn header_fields() {
        // MPEG-1 layer III, 128 kbit/s, 44.1 kHz, joint stereo.
        let h = parse_header(&[0xff, 0xfb, 0x90, 0x44]).unwrap();
        assert_eq!((h.version, h.layer, h.bitrate_kbps, h.sample_rate), (1, 3, 128, 44_100));
        assert_eq!((h.channels, h.length, h.samples), (2, 417, 1152));
        // MPEG-2 layer III, 64 kbit/s, 22.05 kHz, mono, padded.
        let h = parse_header(&[0xff, 0xf3, 0x82, 0xc4]).unwrap();
        assert_eq!((h.version, h.bitrate_kbps, h.sample_rate, h.channels), (2, 64, 22_050, 1));
        assert_eq!((h.length, h.samples), (209, 576));
        for bad in [
            [0xff, 0xfb, 0xf0, 0],
            [0xff, 0xfb, 0x0c, 0],
            [0xff, 0xe9, 0x90, 0],
            [0xfe, 0xfb, 0x90, 0],
        ] {
            assert!(parse_header(&bad).is_none());
        }
    }

    #[test]
    fn constant_bitrate_duration() {
        let mut file = id3v2(3, &[Id3Frame::text("TIT2", "CBR")], false);
        // 100 frames of 1152 samples at 44.1 kHz: 2.612 s.
        file.extend(mpeg_frames(100, false));
        let p = probe(&mut SliceSource(&file)).unwrap();
        assert_eq!(p.tags.title.as_deref(), Some("CBR"));
        assert_eq!(p.audio.bitrate_kbps, 128);
        assert!((2600..2620).contains(&p.audio.duration_ms), "{}", p.audio.duration_ms);
        assert_eq!(p.audio.codec, Codec::Mp3);
    }

    #[test]
    fn xing_header_gives_exact_duration() {
        let file = mpeg_frames(50, true);
        let p = probe(&mut SliceSource(&file)).unwrap();
        // The Xing header claims 1000 frames: 26.122 s.
        assert_eq!(p.audio.duration_ms, 26_122);
    }

    #[test]
    fn false_syncs_in_garbage_are_skipped() {
        let mut file = vec![0xff, 0xfb, 0x90, 0x44, 0, 0, 0xff, 0xe0, 1];
        file.extend(mpeg_frames(10, false));
        let p = probe(&mut SliceSource(&file)).unwrap();
        assert_eq!(p.audio.sample_rate, 44_100);
        assert!(probe(&mut SliceSource(&[0xff, 0xfb, 0, 0, 1, 2, 3])).is_err());
    }
}
