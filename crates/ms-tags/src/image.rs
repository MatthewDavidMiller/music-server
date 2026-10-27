//! Just enough of JPEG and PNG to know an image's type and pixel size, so a
//! cover already small enough is served as-is rather than resized.

use crate::Mime;
use ms_common::buf::Cursor;

pub fn sniff(data: &[u8]) -> Option<Mime> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Mime::Jpeg)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Mime::Png)
    } else if data.starts_with(b"GIF8") {
        Some(Mime::Gif)
    } else if data.starts_with(b"BM") {
        Some(Mime::Bmp)
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some(Mime::Webp)
    } else {
        None
    }
}

/// (width, height) of a JPEG or PNG.
pub fn dimensions(data: &[u8]) -> Option<(u32, u32)> {
    match sniff(data)? {
        Mime::Png => {
            let mut c = Cursor::new(data.get(8..)?);
            c.skip(4)?;
            if c.take(4)? != b"IHDR" {
                return None;
            }
            Some((c.be32()?, c.be32()?))
        }
        Mime::Jpeg => jpeg_dimensions(data),
        _ => None,
    }
}

/// Walks JPEG markers to the first start-of-frame segment.
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut c = Cursor::new(data.get(2..)?);
    for _ in 0..1024 {
        // Fill bytes (0xff) may precede a marker.
        let mut marker = c.u8()?;
        if marker != 0xff {
            return None;
        }
        while marker == 0xff {
            marker = c.u8()?;
        }
        match marker {
            0xd8 | 0x01 | 0xd0..=0xd7 => continue,
            0xd9 | 0xda => return None,
            _ => {}
        }
        let len = usize::from(c.be16()?);
        let body = c.take(len.checked_sub(2)?)?;
        let is_sof = matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_sof {
            let mut s = Cursor::new(body);
            s.skip(1)?;
            let h = s.be16()?;
            let w = s.be16()?;
            return Some((u32::from(w), u32::from(h)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{tiny_jpeg, tiny_png};

    #[test]
    fn sizes() {
        assert_eq!(dimensions(&tiny_png(640, 480)), Some((640, 480)));
        assert_eq!(dimensions(&tiny_jpeg(300, 200)), Some((300, 200)));
        assert_eq!(sniff(b"GIF89a"), Some(Mime::Gif));
        assert_eq!(dimensions(b"GIF89a"), None);
        assert_eq!(dimensions(&[0xff, 0xd8, 0xff, 0xd9]), None);
        assert_eq!(dimensions(b""), None);
    }
}
