//! ID3v1 and v1.1: 128 bytes at the end of an MP3, used only to fill in what
//! an ID3v2 tag did not provide.

use crate::{Result, Source, Tags, genre, text};

pub(crate) const LEN: u64 = 128;

/// The tag at the end of the file, if there is one.
pub(crate) fn read(src: &mut dyn Source) -> Result<Option<Tags>> {
    let size = src.size();
    if size < LEN {
        return Ok(None);
    }
    let raw = src.read_at(size - LEN, 128)?;
    Ok(parse(&raw))
}

fn field(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    text::clean(&text::latin1(&bytes[..end]))
}

pub(crate) fn parse(raw: &[u8]) -> Option<Tags> {
    if raw.len() != 128 || &raw[..3] != b"TAG" {
        return None;
    }
    let mut tags = Tags {
        title: field(&raw[3..33]),
        artist: field(&raw[33..63]),
        album: field(&raw[63..93]),
        year: text::year(&text::latin1(&raw[93..97])),
        ..Tags::default()
    };
    // ID3v1.1 keeps the track number in the comment's last two bytes.
    if raw[125] == 0 && raw[126] != 0 {
        tags.track = Some(u16::from(raw[126]));
    }
    if let Some(g) = genre::name(usize::from(raw[127])) {
        tags.genres.push(g.to_owned());
    }
    Some(tags)
}

#[cfg(test)]
mod tests {
    #[test]
    fn v11_tag() {
        let mut raw = vec![0_u8; 128];
        raw[..3].copy_from_slice(b"TAG");
        raw[3..8].copy_from_slice(b"Title");
        raw[33..39].copy_from_slice(b"Artist");
        raw[93..97].copy_from_slice(b"1987");
        raw[126] = 7;
        raw[127] = 17;
        let t = super::parse(&raw).unwrap();
        assert_eq!(t.title.as_deref(), Some("Title"));
        assert_eq!(t.artist.as_deref(), Some("Artist"));
        assert_eq!(t.album, None);
        assert_eq!((t.year, t.track), (Some(1987), Some(7)));
        assert_eq!(t.genres, ["Rock"]);
        raw[127] = 255;
        assert!(super::parse(&raw).unwrap().genres.is_empty());
        assert!(super::parse(&raw[..127]).is_none());
    }
}
