//! The parsers against real encoder output: ffmpeg writes a three-second
//! tone in every supported container, with tags and (where the muxer
//! supports it) an attached cover, and the probe must read them back.
//!
//! Skipped with a message when ffmpeg is missing, unless MS_REQUIRE_FFMPEG
//! is set (it is in the toolbox), which makes a missing ffmpeg a failure.

#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use ms_tags::{Codec, Container, FileSource, Mime, load_picture, probe};

fn ffmpeg() -> Option<PathBuf> {
    let found = Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success());
    if found {
        return Some(PathBuf::from("ffmpeg"));
    }
    assert!(
        std::env::var_os("MS_REQUIRE_FFMPEG").is_none(),
        "MS_REQUIRE_FFMPEG is set but ffmpeg is missing"
    );
    eprintln!("skipping: ffmpeg is not installed");
    None
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ms-tags-real-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(ffmpeg: &Path, args: &[&str]) {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

const TAGS: &[&str] = &[
    "-metadata",
    "title=Real Title",
    "-metadata",
    "artist=Real Artist",
    "-metadata",
    "album=Real Album",
    "-metadata",
    "album_artist=Real Album Artist",
    "-metadata",
    "track=3/9",
    "-metadata",
    "disc=1/2",
    "-metadata",
    "date=2001",
    "-metadata",
    "genre=Jazz",
];

struct Case {
    file: &'static str,
    codec_args: &'static [&'static str],
    cover: bool,
    container: Container,
    codec: Codec,
    /// Whether the muxer writes album artist and disc (WAV INFO has neither).
    full_tags: bool,
}

#[test]
fn every_container_round_trips_through_ffmpeg() {
    let Some(ff) = ffmpeg() else { return };
    let dir = scratch("all");
    let cover = dir.join("cover.png");
    run(
        &ff,
        &["-f", "lavfi", "-i", "color=c=red:s=96x64", "-frames:v", "1", cover.to_str().unwrap()],
    );
    let cases = [
        Case {
            file: "a.mp3",
            codec_args: &["-c:a", "libmp3lame", "-b:a", "128k", "-id3v2_version", "3"],
            cover: true,
            container: Container::Mp3,
            codec: Codec::Mp3,
            full_tags: true,
        },
        Case {
            file: "b.mp3",
            codec_args: &["-c:a", "libmp3lame", "-q:a", "4", "-id3v2_version", "4"],
            cover: true,
            container: Container::Mp3,
            codec: Codec::Mp3,
            full_tags: true,
        },
        Case {
            file: "c.flac",
            codec_args: &["-c:a", "flac"],
            cover: true,
            container: Container::Flac,
            codec: Codec::Flac,
            full_tags: true,
        },
        Case {
            file: "d.m4a",
            codec_args: &["-c:a", "aac", "-b:a", "128k"],
            cover: true,
            container: Container::Mp4,
            codec: Codec::Aac,
            full_tags: true,
        },
        Case {
            file: "e.m4a",
            codec_args: &["-c:a", "alac"],
            cover: true,
            container: Container::Mp4,
            codec: Codec::Alac,
            full_tags: true,
        },
        Case {
            file: "f.ogg",
            codec_args: &["-c:a", "libvorbis", "-q:a", "3"],
            cover: false,
            container: Container::Ogg,
            codec: Codec::Vorbis,
            full_tags: true,
        },
        Case {
            file: "g.opus",
            codec_args: &["-c:a", "libopus", "-b:a", "96k"],
            cover: false,
            container: Container::Ogg,
            codec: Codec::Opus,
            full_tags: true,
        },
        Case {
            file: "h.wav",
            codec_args: &["-c:a", "pcm_s16le"],
            cover: false,
            container: Container::Wav,
            codec: Codec::Pcm,
            full_tags: false,
        },
    ];
    for case in &cases {
        let out = dir.join(case.file);
        let mut args: Vec<&str> =
            vec!["-f", "lavfi", "-i", "sine=frequency=440:duration=3:sample_rate=44100"];
        if case.cover {
            args.extend([
                "-i",
                cover.to_str().unwrap(),
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:v",
                "png",
                "-disposition:v",
                "attached_pic",
            ]);
        }
        args.extend(["-ac", "2"]);
        args.extend(case.codec_args);
        args.extend(TAGS);
        args.push(out.to_str().unwrap());
        run(&ff, &args);

        let mut src = FileSource::new(File::open(&out).unwrap()).unwrap();
        let p = probe(&mut src).unwrap_or_else(|e| panic!("{}: {e}", case.file));
        let t = &p.tags;
        let name = case.file;
        assert_eq!(p.container, Some(case.container), "{name}");
        assert_eq!(p.audio.codec, case.codec, "{name}");
        assert_eq!(t.title.as_deref(), Some("Real Title"), "{name}");
        assert_eq!(t.artist.as_deref(), Some("Real Artist"), "{name}");
        assert_eq!(t.album.as_deref(), Some("Real Album"), "{name}");
        assert_eq!(t.track, Some(3), "{name}");
        assert_eq!(t.year, Some(2001), "{name}");
        assert_eq!(t.genres, ["Jazz"], "{name}");
        if case.full_tags {
            assert_eq!(t.album_artist.as_deref(), Some("Real Album Artist"), "{name}");
            assert_eq!(t.disc, Some(1), "{name}");
        }
        let ms = p.audio.duration_ms;
        assert!((2900..=3150).contains(&ms), "{name}: duration {ms} ms");
        assert_eq!(p.audio.channels, 2, "{name}");
        assert!(p.audio.sample_rate == 44_100 || p.audio.sample_rate == 48_000, "{name}");
        assert!(p.audio.bitrate_kbps > 0, "{name}");
        if case.cover {
            let at = p.picture.unwrap_or_else(|| panic!("{name}: no picture"));
            let pic = load_picture(&mut src, at).unwrap().unwrap();
            assert_eq!(pic.mime, Mime::Png, "{name}");
            assert_eq!(ms_tags::image::dimensions(&pic.data), Some((96, 64)), "{name}");
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
