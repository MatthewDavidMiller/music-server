//! RIFF WAVE: the `fmt ` and `data` chunks for the stream facts, and the two
//! places taggers put metadata, a `LIST`/`INFO` chunk and an embedded ID3v2
//! tag in an `id3 ` chunk.

use crate::limits::{MAX_FIELD, MAX_ITEMS};
use crate::{
    AudioInfo, Codec, Picture, PictureRef, Probe, Result, SliceSource, Source, average_kbps, id3v2,
    text,
};
use ms_common::buf::Cursor;

struct Walk {
    probe: Probe,
    picture: Option<Picture>,
}

fn walk(src: &mut dyn Source, want_picture: bool) -> Result<Walk> {
    let mut w = Walk { probe: Probe::default(), picture: None };
    let mut pos = 12;
    let mut byte_rate = 0;
    let mut data_len = 0;
    for _ in 0..MAX_ITEMS {
        let h = src.read_at(pos, 8)?;
        if h.len() < 8 {
            break;
        }
        let id: [u8; 4] = h[..4].try_into().unwrap_or([0; 4]);
        let len = u64::from(u32::from_le_bytes(h[4..8].try_into().unwrap_or([0; 4])));
        let body = pos + 8;
        // Chunks are padded to an even length.
        pos = body + len + (len & 1);
        let small = usize::try_from(len).ok().filter(|&n| n <= MAX_FIELD);
        match (&id, small) {
            (b"fmt ", Some(n)) => {
                let fmt = src.read_at(body, n.min(16))?;
                let mut cur = Cursor::new(&fmt);
                let format = cur.le16().unwrap_or(0);
                w.probe.audio.channels = u8::try_from(cur.le16().unwrap_or(0)).unwrap_or(0);
                w.probe.audio.sample_rate = cur.le32().unwrap_or(0);
                byte_rate = cur.le32().unwrap_or(0);
                cur.skip(2);
                w.probe.audio.bit_depth = u8::try_from(cur.le16().unwrap_or(0)).unwrap_or(0);
                w.probe.audio.codec =
                    if matches!(format, 1 | 3 | 0xfffe) { Codec::Pcm } else { Codec::Unknown };
            }
            (b"data", _) => data_len = len,
            (b"LIST", Some(n)) => {
                let list = src.read_at(body, n)?;
                if list.starts_with(b"INFO") {
                    info(&list[4..], &mut w.probe);
                }
            }
            (b"id3 " | b"ID3 ", Some(n)) => {
                let tag = src.read_at(body, n)?;
                if let Some(p) = id3v2::read(&mut SliceSource(&tag), 0, want_picture)? {
                    w.probe.tags.fill_from(p.tags);
                    if p.picture.is_some() {
                        w.probe.picture = Some(PictureRef::Reparse);
                    }
                    w.picture = p.picture_data;
                }
            }
            _ => {}
        }
        if pos >= src.size() {
            break;
        }
    }
    if byte_rate > 0 {
        let ms = data_len.saturating_mul(1000) / u64::from(byte_rate);
        w.probe.audio = AudioInfo {
            duration_ms: ms,
            bitrate_kbps: average_kbps(data_len, ms),
            ..w.probe.audio
        };
    }
    Ok(w)
}

/// The `INFO` list's sub-chunks.
fn info(mut data: &[u8], probe: &mut Probe) {
    for _ in 0..MAX_ITEMS {
        let mut c = Cursor::new(data);
        let (Some(id), Some(len)) = (c.take(4), c.le32()) else { break };
        let Some(len) = usize::try_from(len).ok() else { break };
        let Some(value) = c.take(len) else { break };
        let end = value.iter().position(|&b| b == 0).unwrap_or(value.len());
        let v = text::utf8(&value[..end]);
        let key = match id {
            b"INAM" => "TITLE",
            b"IART" => "ARTIST",
            b"IPRD" => "ALBUM",
            b"IGNR" => "GENRE",
            b"ICRD" => "DATE",
            b"ITRK" | b"IPRT" => "TRACKNUMBER",
            _ => "",
        };
        probe.tags.set_named(key, &v);
        let _ = c.skip(len & 1);
        data = c.rest();
    }
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
    use crate::testgen::{Id3Frame, id3v2, wav};

    #[test]
    fn info_list_and_id3_chunk() {
        let id3 = id3v2(
            3,
            &[Id3Frame::text("TALB", "Wave Album"), Id3Frame::picture(3, "image/png", b"P")],
            false,
        );
        let file =
            wav(44_100, 2, 16, 44_100 * 4 * 2, &[("INAM", "Wave"), ("IART", "Waver")], Some(&id3));
        let mut src = SliceSource(&file);
        let p = probe(&mut src).unwrap();
        assert_eq!(p.tags.title.as_deref(), Some("Wave"));
        assert_eq!(p.tags.artist.as_deref(), Some("Waver"));
        assert_eq!(p.tags.album.as_deref(), Some("Wave Album"));
        assert_eq!(p.audio.duration_ms, 2000);
        assert_eq!(p.audio.bitrate_kbps, 1411);
        assert_eq!((p.audio.bit_depth, p.audio.codec), (16, Codec::Pcm));
        assert_eq!(p.picture, Some(PictureRef::Reparse));
        assert_eq!(picture(&mut src).unwrap().unwrap().data, b"P");
    }
}
