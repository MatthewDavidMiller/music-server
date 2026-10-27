//! Transcoded streams: ffmpeg decodes the file and encodes MP3 or Opus to
//! its stdout, which is copied to the client as it arrives.

use std::ffi::OsString;
use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::time::Duration;

use ms_common::Result;
use ms_http::StreamSource;

use crate::{Permit, Supervised, input_arg};

/// Bitrates a client may ask for (kbit/s); a request is rounded down to
/// one of these, and up to the smallest.
pub const BITRATES: &[u32] = &[32, 48, 64, 96, 128, 160, 192, 256, 320];
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mp3,
    Opus,
}

impl Format {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "mp3" => Some(Format::Mp3),
            "opus" | "ogg" => Some(Format::Opus),
            _ => None,
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Format::Mp3 => "mp3",
            Format::Opus => "opus",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::Mp3 => "audio/mpeg",
            Format::Opus => "audio/ogg",
        }
    }

    /// The highest bitrate worth encoding at.
    fn ceiling(self) -> u32 {
        match self {
            Format::Mp3 => 320,
            Format::Opus => 256,
        }
    }
}

/// `kbps` rounded down to a supported bitrate for `format`.
pub fn clamp_bitrate(format: Format, kbps: u32) -> u32 {
    let wanted = kbps.min(format.ceiling());
    BITRATES.iter().copied().filter(|&b| b <= wanted).max().unwrap_or(BITRATES[0])
}

/// The ffmpeg arguments for one stream.
pub fn arguments(input: &Path, format: Format, kbps: u32, offset_s: u32) -> Vec<OsString> {
    let mut a: Vec<OsString> =
        ["-nostdin", "-hide_banner", "-loglevel", "error", "-protocol_whitelist", "file"]
            .iter()
            .map(OsString::from)
            .collect();
    if offset_s > 0 {
        a.push("-ss".into());
        a.push(offset_s.to_string().into());
    }
    a.push("-i".into());
    a.push(input_arg(input));
    for s in
        ["-map", "0:a:0", "-vn", "-sn", "-dn", "-map_metadata", "-1", "-ac", "2", "-threads", "1"]
    {
        a.push(s.into());
    }
    let bitrate = format!("{}k", clamp_bitrate(format, kbps));
    let tail: &[&str] = match format {
        // No ID3 tag, and no Xing frame: on a pipe its counts would be
        // placeholders the muxer cannot go back and fill in.
        Format::Mp3 => &[
            "-c:a",
            "libmp3lame",
            "-b:a",
            &bitrate,
            "-id3v2_version",
            "0",
            "-write_xing",
            "0",
            "-f",
            "mp3",
            "pipe:1",
        ],
        Format::Opus => &["-c:a", "libopus", "-b:a", &bitrate, "-vbr", "on", "-f", "ogg", "pipe:1"],
    };
    a.extend(tail.iter().map(OsString::from));
    a
}

/// The byte count a client is promised when it asks for an estimated
/// length: duration times bitrate.
pub fn estimated_length(duration_ms: u64, kbps: u32) -> u64 {
    duration_ms.saturating_mul(u64::from(kbps)) / 8
}

/// A running transcode, as a response body.
#[derive(Debug)]
pub struct TranscodeStream {
    child: Supervised,
    /// Exactly this many bytes are sent when set: the output is cut or
    /// padded with zeroes to match a promised Content-Length.
    exact: Option<u64>,
    _permit: Permit,
}

/// What to transcode.
#[derive(Clone, Copy, Debug)]
pub struct Job<'a> {
    pub input: &'a Path,
    pub format: Format,
    pub kbps: u32,
    /// Seconds into the track to start from.
    pub offset_s: u32,
    pub duration_ms: u64,
    /// Send exactly this many bytes (see [`TranscodeStream`]).
    pub exact: Option<u64>,
}

impl TranscodeStream {
    /// Starts ffmpeg. The deadline is twice the remaining duration plus a
    /// minute, long enough for a client that pauses.
    pub fn start(ffmpeg: &Path, job: &Job<'_>, permit: Permit) -> Result<Self> {
        let remaining = job.duration_ms.saturating_sub(u64::from(job.offset_s) * 1000);
        let deadline = Duration::from_millis(remaining.saturating_mul(2)) + Duration::from_secs(60);
        let args = arguments(job.input, job.format, job.kbps, job.offset_s);
        let child = Supervised::spawn(ffmpeg, &args, deadline)?;
        Ok(Self { child, exact: job.exact, _permit: permit })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl StreamSource for TranscodeStream {
    fn pump(&mut self, out: &mut dyn Write) -> io::Result<()> {
        let Some(stdout) = self.child.stdout() else {
            return Err(io::Error::other("ffmpeg has no output"));
        };
        let mut sent: u64 = 0;
        let mut chunk = vec![0_u8; CHUNK];
        loop {
            let n = match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let n = match self.exact {
                Some(limit) => {
                    usize::try_from(limit.saturating_sub(sent)).unwrap_or(usize::MAX).min(n)
                }
                None => n,
            };
            // A failed write is the client leaving; returning the error drops
            // this stream, and with it the ffmpeg process group.
            out.write_all(&chunk[..n])?;
            sent += n as u64;
            if self.exact.is_some_and(|limit| sent >= limit) {
                return Ok(());
            }
        }
        if let Some(limit) = self.exact {
            let zeros = [0_u8; 8192];
            while sent < limit {
                let n = usize::try_from(limit - sent).unwrap_or(usize::MAX).min(zeros.len());
                out.write_all(&zeros[..n])?;
                sent += n as u64;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Slots;
    use crate::testutil::{ffmpeg, scratch, tone};

    fn job(
        input: &Path,
        format: Format,
        kbps: u32,
        offset_s: u32,
        duration_ms: u64,
        exact: Option<u64>,
    ) -> Job<'_> {
        Job { input, format, kbps, offset_s, duration_ms, exact }
    }

    #[test]
    fn bitrates_and_arguments() {
        assert_eq!(clamp_bitrate(Format::Mp3, 0), 32);
        assert_eq!(clamp_bitrate(Format::Mp3, 200), 192);
        assert_eq!(clamp_bitrate(Format::Mp3, 9999), 320);
        assert_eq!(clamp_bitrate(Format::Opus, 320), 256);
        let a = arguments(Path::new("/music/-x.flac"), Format::Opus, 128, 30);
        let text: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert!(text.windows(2).any(|w| w == ["-i", "file:/music/-x.flac"]));
        assert!(text.windows(2).any(|w| w == ["-ss", "30"]));
        assert!(text.windows(2).any(|w| w == ["-protocol_whitelist", "file"]));
        assert_eq!(text.last().unwrap(), "pipe:1");
        assert_eq!(estimated_length(10_000, 128), 160_000);
        assert_eq!(Format::parse("OGG"), Some(Format::Opus));
        assert_eq!(Format::parse("raw"), None);
    }

    #[test]
    fn mp3_and_opus_streams() {
        let Some(ff) = ffmpeg() else { return };
        let dir = scratch("streams");
        let src = dir.join("tone.flac");
        tone(&ff, &src, 4);
        let slots = Slots::new(2);
        for (format, magic) in [(Format::Mp3, &b""[..]), (Format::Opus, b"OggS")] {
            let mut s = TranscodeStream::start(
                &ff,
                &job(&src, format, 96, 0, 4000, None),
                slots.try_acquire().unwrap(),
            )
            .unwrap();
            let mut out = Vec::new();
            s.pump(&mut out).unwrap();
            assert!(out.len() > 10_000, "{format:?}: {} bytes", out.len());
            if format == Format::Mp3 {
                // A frame sync straight away: no tag, no Xing frame.
                assert!(out[0] == 0xff && out[1] & 0xe0 == 0xe0, "{:?}", &out[..4]);
            } else {
                assert!(out.starts_with(magic));
                assert!(out.windows(8).any(|w| w == b"OpusHead"));
            }
        }
        assert_eq!(slots.active(), 0);
        // A promised length is met exactly, by cutting or padding.
        for exact in [1000_u64, 400_000] {
            let mut s = TranscodeStream::start(
                &ff,
                &job(&src, Format::Mp3, 64, 2, 4000, Some(exact)),
                slots.try_acquire().unwrap(),
            )
            .unwrap();
            let mut out = Vec::new();
            s.pump(&mut out).unwrap();
            assert_eq!(out.len() as u64, exact);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A writer that fails after a few chunks, like a client that hung up.
    struct HangUp(usize);

    impl Write for HangUp {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::new(ErrorKind::BrokenPipe, "gone"));
            }
            self.0 -= 1;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_client_hanging_up_kills_ffmpeg_and_frees_the_slot() {
        let Some(ff) = ffmpeg() else { return };
        let dir = scratch("hangup");
        let src = dir.join("long.flac");
        tone(&ff, &src, 120);
        let slots = Slots::new(1);
        let mut s = TranscodeStream::start(
            &ff,
            &job(&src, Format::Mp3, 320, 0, 120_000, None),
            slots.try_acquire().unwrap(),
        )
        .unwrap();
        let pid = s.id();
        assert!(slots.try_acquire().is_none());
        assert!(s.pump(&mut HangUp(2)).is_err());
        drop(s);
        assert_eq!(slots.active(), 0);
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "ffmpeg {pid} still exists");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
