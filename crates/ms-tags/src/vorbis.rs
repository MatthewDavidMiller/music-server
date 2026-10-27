//! Vorbis comments (shared by FLAC, Ogg Vorbis, and Ogg Opus) and the FLAC
//! PICTURE structure, which Ogg files also carry base64-encoded in a
//! `METADATA_BLOCK_PICTURE` comment.

use crate::limits::MAX_ITEMS;
use crate::{Mime, Picture, Tags, text};
use ms_common::buf::Cursor;

/// What a comment block held.
#[derive(Debug, Default)]
pub(crate) struct Comments {
    pub(crate) tags: Tags,
    pub(crate) has_picture: bool,
    /// The best picture, when decoding was asked for.
    pub(crate) picture: Option<Picture>,
}

/// Parses a comment block (vendor string, count, `KEY=value` entries). A
/// truncated block yields whatever entries were complete.
pub(crate) fn parse(data: &[u8], want_picture: bool) -> Comments {
    let mut out = Comments::default();
    let mut best_type: Option<u32> = None;
    let mut c = Cursor::new(data);
    let Some(vendor) = c.le32().and_then(|n| usize::try_from(n).ok()) else { return out };
    if c.skip(vendor).is_none() {
        return out;
    }
    let Some(count) = c.le32() else { return out };
    for _ in 0..count.min(u32::try_from(MAX_ITEMS).unwrap_or(u32::MAX)) {
        let Some(len) = c.le32().and_then(|n| usize::try_from(n).ok()) else { break };
        let Some(entry) = c.take(len) else { break };
        let Some(eq) = entry.iter().position(|&b| b == b'=') else { continue };
        let key = text::latin1(&entry[..eq]);
        let value = &entry[eq + 1..];
        let upper = key.to_ascii_uppercase();
        if upper == "METADATA_BLOCK_PICTURE" || upper == "COVERART" {
            out.has_picture = true;
            if want_picture && let Some(raw) = ms_common::base64::decode(value) {
                let decoded = if upper == "COVERART" {
                    Some((3, Picture { mime: Mime::Other, data: raw }))
                } else {
                    picture_block(&raw).map(|(ptype, mime, at, len)| {
                        (ptype, Picture { mime, data: raw[at..at + len].to_vec() })
                    })
                };
                if let Some((ptype, pic)) = decoded
                    && (best_type.is_none() || (best_type != Some(3) && ptype == 3))
                {
                    best_type = Some(ptype);
                    out.picture = Some(pic);
                }
            }
            continue;
        }
        out.tags.set_named(&key, &text::utf8(value));
    }
    out
}

/// Parses a FLAC PICTURE structure: its picture type, MIME type, and the
/// offset and length of the image within `block`. Only the bytes before the
/// image need be present for the offset; the length is checked against
/// `block` only when `complete`.
pub(crate) fn picture_header(block: &[u8], complete: bool) -> Option<(u32, Mime, usize, usize)> {
    let mut c = Cursor::new(block);
    let ptype = c.be32()?;
    let mime_len = usize::try_from(c.be32()?).ok()?;
    let mime = Mime::from_name(&text::latin1(c.take(mime_len)?));
    let desc_len = usize::try_from(c.be32()?).ok()?;
    c.skip(desc_len)?;
    c.skip(16)?; // width, height, depth, colours
    let len = usize::try_from(c.be32()?).ok()?;
    let at = block.len() - c.remaining();
    if complete && at.checked_add(len)? > block.len() {
        return None;
    }
    Some((ptype, mime, at, len))
}

fn picture_block(block: &[u8]) -> Option<(u32, Mime, usize, usize)> {
    picture_header(block, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{flac_picture, vorbis_comments};

    #[test]
    fn comments_and_pictures() {
        let pic = flac_picture(3, "image/png", b"PNGDATA");
        let mut entries = vec![
            "TITLE=Song".to_owned(),
            "artist=Band".to_owned(),
            "TRACKNUMBER=2".to_owned(),
            "GENRE=Jazz".to_owned(),
            "GENRE=Funk".to_owned(),
            "noequals".to_owned(),
        ];
        entries.push(format!("METADATA_BLOCK_PICTURE={}", ms_common::base64::encode(&pic)));
        let block = vorbis_comments(&entries);
        let c = parse(&block, true);
        assert_eq!(c.tags.title.as_deref(), Some("Song"));
        assert_eq!(c.tags.artist.as_deref(), Some("Band"));
        assert_eq!(c.tags.track, Some(2));
        assert_eq!(c.tags.genres, ["Jazz", "Funk"]);
        assert!(c.has_picture);
        assert_eq!(c.picture.unwrap().data, b"PNGDATA");
        // Truncation keeps the complete entries.
        let short = parse(&block[..block.len() / 2], false);
        assert_eq!(short.tags.title.as_deref(), Some("Song"));
    }

    #[test]
    fn picture_structure_bounds() {
        let pic = flac_picture(3, "image/jpeg", b"JPEG");
        assert_eq!(picture_header(&pic, true), Some((3, Mime::Jpeg, pic.len() - 4, 4)));
        assert!(picture_header(&pic[..pic.len() - 1], true).is_none());
        assert!(picture_header(&pic[..pic.len() - 1], false).is_some());
        assert!(picture_header(&[0, 0, 0, 3, 0xff, 0xff, 0xff, 0xff], true).is_none());
    }
}
