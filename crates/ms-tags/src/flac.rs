//! Native FLAC: `fLaC`, then metadata blocks (STREAMINFO, VORBIS_COMMENT,
//! PICTURE, ...), then audio frames. Blocks other than those three are
//! skipped by length, and a PICTURE block's image is located, not read.

use crate::limits::{MAX_FIELD, MAX_ITEMS, MAX_PICTURE};
use crate::{
    AudioInfo, Codec, Error, Picture, PictureRef, Probe, Result, Source, average_kbps, id3v2,
    samples_to_ms, vorbis,
};

const STREAMINFO: u8 = 0;
const VORBIS_COMMENT: u8 = 4;
const PICTURE: u8 = 6;
/// What is read of a PICTURE block to locate its image.
const PICTURE_HEADER_READ: usize = 64 * 1024;

struct Walk {
    probe: Probe,
    picture: Option<Picture>,
}

fn walk(src: &mut dyn Source, want_picture: bool) -> Result<Walk> {
    let mut w = Walk { probe: Probe::default(), picture: None };
    // A leading ID3v2 tag is not part of FLAC, but some taggers add one.
    let start = id3v2::skip_tags(src, 0)?;
    if src.read_at(start, 4)? != b"fLaC" {
        return Err(Error::new("not a FLAC stream"));
    }
    let mut pos = start + 4;
    let mut best_type: Option<u32> = None;
    let mut total_samples = 0;
    for _ in 0..MAX_ITEMS {
        let h = src.read_at(pos, 4)?;
        let [flags, l0, l1, l2] =
            *h.first_chunk::<4>().ok_or_else(|| Error::new("truncated FLAC metadata"))?;
        let last = flags & 0x80 != 0;
        let kind = flags & 0x7f;
        let len = usize::from(l0) << 16 | usize::from(l1) << 8 | usize::from(l2);
        let body = pos + 4;
        pos = body + len as u64;
        match kind {
            STREAMINFO => {
                let s = src.read_at(body, len.min(34))?;
                if s.len() >= 18 {
                    let x = u64::from_be_bytes(s[10..18].try_into().unwrap_or([0; 8]));
                    let rate = u32::try_from(x >> 44).unwrap_or(0);
                    let channels = u8::try_from((x >> 41) & 7).unwrap_or(0) + 1;
                    let bits = u8::try_from((x >> 36) & 31).unwrap_or(0) + 1;
                    total_samples = x & 0xf_ffff_ffff;
                    w.probe.audio = AudioInfo {
                        duration_ms: 0,
                        bitrate_kbps: 0,
                        sample_rate: rate,
                        channels,
                        bit_depth: bits,
                        codec: Codec::Flac,
                    };
                }
            }
            VORBIS_COMMENT if len <= MAX_FIELD * 16 => {
                let data = src.read_at(body, len)?;
                let c = vorbis::parse(&data, false);
                w.probe.tags.fill_from(c.tags);
            }
            PICTURE => {
                let head = src.read_at(body, len.min(PICTURE_HEADER_READ))?;
                if let Some((ptype, mime, at, plen)) = vorbis::picture_header(&head, false)
                    && at + plen <= len
                    && (best_type.is_none() || (best_type != Some(3) && ptype == 3))
                {
                    best_type = Some(ptype);
                    w.probe.picture = Some(PictureRef::Slice {
                        offset: body + at as u64,
                        len: u32::try_from(plen).unwrap_or(0),
                        mime,
                    });
                    if want_picture && plen <= MAX_PICTURE {
                        let data = src.read_at(body + at as u64, plen)?;
                        w.picture = Some(Picture { data, mime });
                    }
                }
            }
            _ => {}
        }
        if last || pos >= src.size() {
            break;
        }
    }
    let ms = samples_to_ms(total_samples, w.probe.audio.sample_rate);
    w.probe.audio.duration_ms = ms;
    w.probe.audio.bitrate_kbps = average_kbps(src.size().saturating_sub(pos), ms);
    // Old taggers sometimes left an ID3v2 tag in front; let it fill gaps.
    if start > 0
        && let Some(p) = id3v2::read(src, 0, false)?
    {
        w.probe.tags.fill_from(p.tags);
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
    use crate::testgen::{flac, flac_picture, vorbis_comments};
    use crate::{SliceSource, load_picture};

    #[test]
    fn streaminfo_comments_and_picture() {
        let comments = vorbis_comments(&["TITLE=Lossless".into(), "DATE=2010".into()]);
        let pic = flac_picture(3, "image/jpeg", b"\xff\xd8JPEG");
        // 10 s at 48 kHz, stereo, 24-bit, with 100 000 bytes of "audio".
        let file = flac(48_000, 2, 24, 480_000, &comments, Some(&pic), 100_000);
        let mut src = SliceSource(&file);
        let p = probe(&mut src).unwrap();
        assert_eq!(p.tags.title.as_deref(), Some("Lossless"));
        assert_eq!(p.tags.year, Some(2010));
        assert_eq!((p.audio.sample_rate, p.audio.channels, p.audio.bit_depth), (48_000, 2, 24));
        assert_eq!(p.audio.duration_ms, 10_000);
        assert_eq!(p.audio.bitrate_kbps, 80);
        let pic = load_picture(&mut src, p.picture.unwrap()).unwrap().unwrap();
        assert_eq!(pic.data, b"\xff\xd8JPEG");
        assert_eq!(picture(&mut src).unwrap().unwrap().data, b"\xff\xd8JPEG");
    }

    #[test]
    fn not_flac() {
        assert!(probe(&mut SliceSource(b"fLaX")).is_err());
    }
}
