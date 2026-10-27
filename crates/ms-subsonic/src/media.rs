//! The calls that answer with bytes: `stream`, `download`, and
//! `getCoverArt`.
//!
//! `stream` sends the original file (with Range support, so players can
//! seek and resume) unless the client asks for a format or a bitrate
//! ceiling the file does not meet; then ffmpeg transcodes it, or, when
//! every transcode slot is busy and the client did not insist on a format,
//! the original is sent anyway.

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use ms_common::fsio;
use ms_http::{Request, Response, percent_encode};
use ms_library::{Cover, Entity, Id, Kind, Library, Track};
use ms_media::cover::{self, bucket};
use ms_media::transcode::{Format as AudioFormat, Job, TranscodeStream, estimated_length};
use ms_tags::{Codec, Container};
use ms_tags::{FileSource, Mime, image, limits::MAX_PICTURE, load_picture};

use crate::covers::CoverCache;
use crate::error::{ApiError, ApiResult};
use crate::model::content_type;
use crate::params::Params;
use crate::{Api, Reply, User};

/// What a cover request resolves to.
enum Source {
    /// An image file, relative to the music directory.
    Folder(String),
    /// A track's embedded picture.
    Embedded(u32),
}

fn source_of(cover: &Cover) -> Option<Source> {
    match cover {
        Cover::None => None,
        Cover::Folder(path) => Some(Source::Folder(path.clone())),
        Cover::Embedded(t) => Some(Source::Embedded(*t)),
    }
}

fn not_modified(req: &Request, etag: &str) -> bool {
    req.header("if-none-match").is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
}

fn image_response(req: &Request, data: Vec<u8>, mime: Mime, etag: &str) -> Response {
    let headers = |r: Response| {
        r.with_header("Cache-Control", "private, max-age=86400".to_owned())
            .with_header("ETag", etag.to_owned())
    };
    if not_modified(req, etag) {
        return headers(Response::new(304, mime.as_str(), Vec::new()));
    }
    headers(Response::new(200, mime.as_str(), data))
}

impl Api {
    fn music_path(&self, rel: &str) -> PathBuf {
        self.library.config().music_dir.join(rel)
    }

    /// The track's file, opened without following links, and its size.
    fn open_track(&self, track: &Track) -> ApiResult<(File, u64)> {
        let file = fsio::open_nofollow(self.music_path(&track.rec.path))
            .map_err(|_| ApiError::not_found("Song file"))?;
        let meta = file.metadata().map_err(|_| ApiError::not_found("Song file"))?;
        if !meta.is_file() {
            return Err(ApiError::not_found("Song file"));
        }
        Ok((file, meta.len()))
    }

    fn track_for(lib: &Library, p: &Params<'_>) -> ApiResult<u32> {
        match lib.lookup(&p.id("id")?) {
            Some(Entity::Track(t)) => Ok(t),
            _ => Err(ApiError::not_found("Song")),
        }
    }

    pub(crate) fn stream(
        &self,
        p: &Params<'_>,
        user: &User,
        req: &Request,
        client: &str,
    ) -> ApiResult<Reply> {
        let lib = self.library.library();
        let t = Self::track_for(&lib, p)?;
        let track = lib.track(t);
        let r = &track.rec;
        let requested = match p.str("format").map(str::trim) {
            None | Some("") => None,
            Some("raw") => {
                return self.raw(req, track, false);
            }
            Some(f) => Some(AudioFormat::parse(f).ok_or_else(|| ApiError::bad_param("format"))?),
        };
        let max_kbps = u32::try_from(p.int("maxBitRate")?.unwrap_or(0).max(0)).unwrap_or(u32::MAX);
        let offset = u32::try_from(p.int("timeOffset")?.unwrap_or(0).max(0)).unwrap_or(0);
        let target = requested.unwrap_or(self.settings.default_format);
        let same_codec = matches!(
            (target, r.container, r.codec),
            (AudioFormat::Mp3, Container::Mp3, Codec::Mp3)
                | (AudioFormat::Opus, Container::Ogg, Codec::Opus)
        );
        let over_ceiling = max_kbps > 0 && r.bitrate_kbps > max_kbps;
        let transcode = match requested {
            Some(_) => !same_codec || over_ceiling,
            None => over_ceiling,
        };
        if offset == 0 && req.header("range").is_none_or(|v| v.starts_with("bytes=0-")) {
            self.set_now_playing(&user.name, track.id, client);
        }
        if !transcode {
            return self.raw(req, track, false);
        }
        let kbps = if max_kbps > 0 { max_kbps } else { self.settings.default_bitrate };
        let permit = if self.media.can_transcode(target) {
            self.media.transcodes.try_acquire()
        } else {
            None
        };
        let Some(permit) = permit else {
            if requested.is_none() {
                return self.raw(req, track, false);
            }
            return Ok(Reply::Raw(
                Response::text(503, "all transcoders are busy")
                    .with_header("Retry-After", "5".to_owned()),
            ));
        };
        let kbps = ms_media::transcode::clamp_bitrate(target, kbps);
        let remaining = r.duration_ms.saturating_sub(u64::from(offset) * 1000);
        let exact = p
            .bool("estimateContentLength")
            .unwrap_or(false)
            .then(|| estimated_length(remaining, kbps));
        let path = self.music_path(&r.path);
        // Opened here first so a missing or linked file fails as a Subsonic
        // error rather than as an ffmpeg one.
        drop(self.open_track(track)?);
        let job = Job {
            input: &path,
            format: target,
            kbps,
            offset_s: offset,
            duration_ms: r.duration_ms,
            exact,
        };
        let stream = TranscodeStream::start(&self.media.ffmpeg, &job, permit)?;
        self.stats.transcodes.fetch_add(1, Ordering::Relaxed);
        self.stats.streams.fetch_add(1, Ordering::Relaxed);
        Ok(Reply::Raw(
            Response::stream(target.content_type(), Box::new(stream), exact).with_header(
                "X-Content-Duration",
                format!("{}.{:03}", remaining / 1000, remaining % 1000),
            ),
        ))
    }

    fn raw(&self, req: &Request, track: &Track, attachment: bool) -> ApiResult<Reply> {
        let (file, size) = self.open_track(track)?;
        self.stats.streams.fetch_add(1, Ordering::Relaxed);
        let mut r = Response::file(req, content_type(track.rec.container), file, size);
        if attachment {
            let name = track.rec.path.rsplit('/').next().unwrap_or("track");
            r = r.with_header(
                "Content-Disposition",
                format!("attachment; filename*=UTF-8''{}", percent_encode(name)),
            );
        }
        Ok(Reply::Raw(r))
    }

    pub(crate) fn download(&self, p: &Params<'_>, req: &Request) -> ApiResult<Reply> {
        let lib = self.library.library();
        let t = Self::track_for(&lib, p)?;
        self.raw(req, lib.track(t), true)
    }

    /// What `id` shows as its cover, if anything.
    fn cover_source(&self, lib: &Library, id: &Id, user: &User) -> ApiResult<Option<Source>> {
        Ok(match (id.kind, lib.lookup(id)) {
            (_, Some(Entity::Album(a))) => source_of(&lib.album(a).cover),
            (_, Some(Entity::Track(t))) => {
                let track = lib.track(t);
                if track.rec.picture.is_some() {
                    Some(Source::Embedded(t))
                } else {
                    source_of(&lib.album(track.album).cover)
                }
            }
            (_, Some(Entity::Artist(a))) => source_of(&lib.artist(a).cover),
            (_, Some(Entity::Dir(d))) => source_of(&lib.dir(d).cover),
            (Kind::Playlist, None) => {
                let (_, pl) = self.find_playlist(user, &id.to_string())?;
                pl.entries.iter().filter_map(|e| Id::parse(e).ok()).find_map(|tid| {
                    match lib.lookup(&tid) {
                        Some(Entity::Track(t)) => source_of(&lib.album(lib.track(t).album).cover),
                        _ => None,
                    }
                })
            }
            _ => None,
        })
    }

    /// The original image bytes and a stamp of their source file.
    fn original(&self, lib: &Library, src: &Source) -> ApiResult<(Vec<u8>, Mime, (u64, i64))> {
        match src {
            Source::Folder(rel) => {
                let path = self.music_path(rel);
                let meta = std::fs::symlink_metadata(&path)
                    .map_err(|_| ApiError::not_found("Cover art"))?;
                let data = fsio::read_bounded(&path, MAX_PICTURE)?
                    .ok_or_else(|| ApiError::not_found("Cover art"))?;
                let mime = image::sniff(&data).unwrap_or(Mime::Other);
                Ok((data, mime, (meta.len(), meta.mtime())))
            }
            Source::Embedded(t) => {
                let track = lib.track(*t);
                let at = track.rec.picture.ok_or_else(|| ApiError::not_found("Cover art"))?;
                let (file, _) = self.open_track(track)?;
                let mut source = FileSource::new(file)?;
                let pic = load_picture(&mut source, at)?
                    .ok_or_else(|| ApiError::not_found("Cover art"))?;
                Ok((pic.data, pic.mime, (track.rec.size, track.rec.mtime_ns)))
            }
        }
    }

    pub(crate) fn get_cover_art(
        &self,
        p: &Params<'_>,
        req: &Request,
        user: &User,
    ) -> ApiResult<Reply> {
        let id = p.id("id")?;
        let size = p.int("size")?.and_then(|s| u32::try_from(s).ok()).filter(|&s| s > 0);
        let lib = self.library.library();
        let src =
            self.cover_source(&lib, &id, user)?.ok_or_else(|| ApiError::not_found("Cover art"))?;
        let source_name = match &src {
            Source::Folder(rel) => format!("folder:{rel}"),
            Source::Embedded(t) => format!("track:{}", lib.track(*t).rec.path),
        };
        let (data, mime, stamp) = self.original(&lib, &src)?;
        let wanted = size.map(bucket);
        let fits = |b: u32| image::dimensions(&data).is_some_and(|(w, h)| w.max(h) <= b);
        let Some(b) = wanted.filter(|&b| !fits(b)) else {
            let etag = format!("\"{}\"", CoverCache::key(&source_name, stamp, 0));
            return Ok(Reply::Raw(image_response(req, data, mime, &etag)));
        };
        let key = CoverCache::key(&source_name, stamp, b);
        let etag = format!("\"{key}\"");
        if let Some(cached) = self.covers.get(&key) {
            return Ok(Reply::Raw(image_response(req, cached, Mime::Jpeg, &etag)));
        }
        let permit = if self.media.can_resize() { self.media.resizes.try_acquire() } else { None };
        let Some(_permit) = permit else {
            // Busy or unable: the original is better than nothing.
            let etag = format!("\"{}\"", CoverCache::key(&source_name, stamp, 0));
            return Ok(Reply::Raw(image_response(req, data, mime, &etag)));
        };
        let resized = match &src {
            Source::Folder(rel) => cover::resize(&self.media.ffmpeg, &self.music_path(rel), b),
            Source::Embedded(_) => {
                let staged = self
                    .media
                    .scratch
                    .join(format!("cover-{}.img", ms_common::rand::random_hex(12)?));
                fsio::atomic_replace(&staged, &data, MAX_PICTURE)?;
                let result = cover::resize(&self.media.ffmpeg, &staged, b);
                let _ = std::fs::remove_file(&staged);
                result
            }
        };
        match resized {
            Ok(jpeg) => {
                if let Err(e) = self.covers.put(&key, &jpeg) {
                    eprintln!("music-server: cannot cache a resized cover: {e}");
                }
                Ok(Reply::Raw(image_response(req, jpeg, Mime::Jpeg, &etag)))
            }
            Err(e) => {
                eprintln!("music-server: {e}");
                let etag = format!("\"{}\"", CoverCache::key(&source_name, stamp, 0));
                Ok(Reply::Raw(image_response(req, data, mime, &etag)))
            }
        }
    }
}
