//! Cover art resized for a client's requested size: ffmpeg scales the image
//! to fit a square and encodes a JPEG. Callers cache the result, so each
//! cover and size is resized once.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use ms_common::{Error, Result};

use crate::{Supervised, input_arg, read_all_bounded};

/// Sizes a request is rounded up to, so a handful of cache entries serve
/// every client.
pub const BUCKETS: &[u32] = &[64, 128, 256, 384, 512, 768, 1024];
pub const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(15);

/// The smallest bucket at least `size`, or the largest.
pub fn bucket(size: u32) -> u32 {
    BUCKETS.iter().copied().find(|&b| b >= size).unwrap_or(BUCKETS[BUCKETS.len() - 1])
}

pub fn arguments(input: &Path, size: u32) -> Vec<OsString> {
    let scale = format!("scale={size}:{size}:force_original_aspect_ratio=decrease");
    let mut a: Vec<OsString> =
        ["-nostdin", "-hide_banner", "-loglevel", "error", "-protocol_whitelist", "file", "-i"]
            .iter()
            .map(OsString::from)
            .collect();
    a.push(input_arg(input));
    for s in
        ["-vf", &scale, "-frames:v", "1", "-c:v", "mjpeg", "-q:v", "4", "-f", "image2", "pipe:1"]
    {
        a.push(s.into());
    }
    a
}

/// The image at `input`, scaled to fit `size` x `size`, as JPEG bytes.
pub fn resize(ffmpeg: &Path, input: &Path, size: u32) -> Result<Vec<u8>> {
    let mut child = Supervised::spawn(ffmpeg, &arguments(input, size), DEADLINE)?;
    let out = match child.stdout() {
        Some(o) => read_all_bounded(o, MAX_OUTPUT),
        None => Err(Error::new("ffmpeg has no output")),
    };
    let (status, err) = child.finish()?;
    let out = out?;
    if !status.success() || !out.starts_with(&[0xff, 0xd8]) {
        return Err(Error::new(format!("cannot resize cover art: {err}")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ffmpeg, scratch};

    #[test]
    fn buckets() {
        assert_eq!(bucket(0), 64);
        assert_eq!(bucket(64), 64);
        assert_eq!(bucket(65), 128);
        assert_eq!(bucket(5000), 1024);
    }

    #[test]
    fn resizes_to_fit() {
        let Some(ff) = ffmpeg() else { return };
        let dir = scratch("cover");
        let src = dir.join("big.png");
        let status = std::process::Command::new(&ff)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=s=600x300",
                "-frames:v",
                "1",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());
        let jpeg = resize(&ff, &src, 128).unwrap();
        let size = jpeg_size(&jpeg);
        assert_eq!(size, Some((128, 64)));
        assert!(resize(&ff, &dir.join("missing.png"), 128).is_err());
        std::fs::write(dir.join("junk.png"), b"not an image").unwrap();
        assert!(resize(&ff, &dir.join("junk.png"), 128).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The SOF0 dimensions of a baseline JPEG (ms-tags has the real parser;
    /// this crate does not depend on it).
    fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
        let i = data.windows(2).position(|w| w == [0xff, 0xc0])?;
        let h = u32::from(u16::from_be_bytes([data[i + 5], data[i + 6]]));
        let w = u32::from(u16::from_be_bytes([data[i + 7], data[i + 8]]));
        Some((w, h))
    }
}
