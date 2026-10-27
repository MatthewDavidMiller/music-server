//! Builders for synthetic audio files, so tests need no binary fixtures.
//! The files are structurally valid (tags, headers, chunk and atom layout)
//! but carry silence or zeroes for audio; the ffmpeg-backed tests elsewhere
//! cover real encoder output.

#![allow(
    clippy::cast_possible_truncation,
    clippy::missing_panics_doc,
    clippy::unwrap_used,
    clippy::trivially_copy_pass_by_ref
)]

/// One ID3v2 frame, named by its ID3v2.3/2.4 identifier.
#[derive(Clone, Debug)]
pub struct Id3Frame {
    id: String,
    data: Vec<u8>,
}

impl Id3Frame {
    /// A text frame (ISO-8859-1 for ASCII text, UTF-8 otherwise).
    pub fn text(id: &str, value: &str) -> Self {
        let mut data = vec![if value.is_ascii() { 0 } else { 3 }];
        data.extend_from_slice(value.as_bytes());
        Self { id: id.to_owned(), data }
    }

    pub fn raw(id: &str, data: Vec<u8>) -> Self {
        Self { id: id.to_owned(), data }
    }

    /// An APIC frame with an empty description.
    pub fn picture(picture_type: u8, mime: &str, image: &[u8]) -> Self {
        let mut data = vec![0];
        data.extend_from_slice(mime.as_bytes());
        data.push(0);
        data.push(picture_type);
        data.push(0);
        data.extend_from_slice(image);
        Self { id: "APIC".to_owned(), data }
    }
}

fn syncsafe(n: usize) -> [u8; 4] {
    [(n >> 21 & 0x7f) as u8, (n >> 14 & 0x7f) as u8, (n >> 7 & 0x7f) as u8, (n & 0x7f) as u8]
}

fn v22_id(id: &str) -> &'static str {
    match id {
        "TIT2" => "TT2",
        "TPE1" => "TP1",
        "TPE2" => "TP2",
        "TALB" => "TAL",
        "TRCK" => "TRK",
        "TPOS" => "TPA",
        "TYER" | "TDRC" => "TYE",
        "TCON" => "TCO",
        "TXXX" => "TXX",
        "APIC" => "PIC",
        _ => "XXX",
    }
}

/// A whole ID3v2 tag. With `unsync`, the body is unsynchronised (every 0xFF
/// gets a following 0x00) and the tag flag set, as ID3v2.3 writers do.
pub fn id3v2(major: u8, frames: &[Id3Frame], unsync: bool) -> Vec<u8> {
    let mut body = Vec::new();
    for f in frames {
        let mut data = f.data.clone();
        if major == 2 {
            if f.id == "APIC" {
                // v2.2 PIC: a three-letter image format instead of a MIME type.
                let mime_end = data[1..].iter().position(|&b| b == 0).unwrap() + 1;
                let fmt: &[u8] = if data[1..mime_end].ends_with(b"png") { b"PNG" } else { b"JPG" };
                let mut pic = vec![data[0]];
                pic.extend_from_slice(fmt);
                pic.extend_from_slice(&data[mime_end + 1..]);
                data = pic;
            }
            body.extend_from_slice(v22_id(&f.id).as_bytes());
            let n = data.len();
            body.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        } else {
            body.extend_from_slice(f.id.as_bytes());
            if major == 4 {
                body.extend_from_slice(&syncsafe(data.len()));
            } else {
                body.extend_from_slice(&(data.len() as u32).to_be_bytes());
            }
            body.extend_from_slice(&[0, 0]);
        }
        body.extend_from_slice(&data);
    }
    body.extend_from_slice(&[0; 16]); // padding
    if unsync {
        let mut out = Vec::with_capacity(body.len() * 2);
        for b in body {
            out.push(b);
            if b == 0xff {
                out.push(0);
            }
        }
        body = out;
    }
    let mut tag = b"ID3".to_vec();
    tag.extend_from_slice(&[major, 0, if unsync { 0x80 } else { 0 }]);
    tag.extend_from_slice(&syncsafe(body.len()));
    tag.extend(body);
    tag
}

/// `count` MPEG-1 layer III frames at 128 kbit/s, 44.1 kHz, stereo. With
/// `xing`, the first carries a Xing header claiming 1000 frames.
pub fn mpeg_frames(count: usize, xing: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(count * 417);
    for i in 0..count {
        let mut frame = vec![0_u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x44]);
        if i == 0 && xing {
            frame[36..40].copy_from_slice(b"Xing");
            frame[40..44].copy_from_slice(&3_u32.to_be_bytes());
            frame[44..48].copy_from_slice(&1000_u32.to_be_bytes());
            frame[48..52].copy_from_slice(&(1000 * 417_u32).to_be_bytes());
        }
        out.extend(frame);
    }
    out
}

pub fn vorbis_comments(entries: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&2_u32.to_le_bytes());
    out.extend_from_slice(b"ms");
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        out.extend_from_slice(&(e.len() as u32).to_le_bytes());
        out.extend_from_slice(e.as_bytes());
    }
    out
}

/// A FLAC PICTURE structure (also the payload of METADATA_BLOCK_PICTURE).
pub fn flac_picture(picture_type: u32, mime: &str, image: &[u8]) -> Vec<u8> {
    let mut out = picture_type.to_be_bytes().to_vec();
    out.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    out.extend_from_slice(mime.as_bytes());
    out.extend_from_slice(&4_u32.to_be_bytes());
    out.extend_from_slice(b"desc");
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(&(image.len() as u32).to_be_bytes());
    out.extend_from_slice(image);
    out
}

fn flac_block(out: &mut Vec<u8>, kind: u8, last: bool, body: &[u8]) {
    out.push(kind | if last { 0x80 } else { 0 });
    let n = body.len();
    out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    out.extend_from_slice(body);
}

/// A FLAC file: STREAMINFO, a padding block, the comments, an optional
/// PICTURE block, and `audio_len` zero bytes of "frames".
pub fn flac(
    rate: u32,
    channels: u8,
    bits: u8,
    total_samples: u64,
    comments: &[u8],
    picture: Option<&[u8]>,
    audio_len: usize,
) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    let mut info = vec![0_u8; 34];
    info[..2].copy_from_slice(&4096_u16.to_be_bytes());
    info[2..4].copy_from_slice(&4096_u16.to_be_bytes());
    let packed = u64::from(rate) << 44
        | u64::from(channels - 1) << 41
        | u64::from(bits - 1) << 36
        | (total_samples & 0xf_ffff_ffff);
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    flac_block(&mut out, 0, false, &info);
    flac_block(&mut out, 1, false, &[0; 32]);
    flac_block(&mut out, 4, picture.is_none(), comments);
    if let Some(p) = picture {
        flac_block(&mut out, 6, true, p);
    }
    out.extend(std::iter::repeat_n(0_u8, audio_len));
    out
}

fn ogg_page(out: &mut Vec<u8>, flags: u8, granule: u64, seq: u32, segments: &[u8], data: &[u8]) {
    out.extend_from_slice(b"OggS");
    out.push(0);
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&0x1234_u32.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&[0; 4]); // checksum, not verified
    out.push(segments.len() as u8);
    out.extend_from_slice(segments);
    out.extend_from_slice(data);
}

/// Lays `packet` out over as many pages as it needs.
fn ogg_packet(out: &mut Vec<u8>, packet: &[u8], first_flags: u8, granule: u64, seq: &mut u32) {
    let mut lacing: Vec<u8> = vec![255; packet.len() / 255];
    lacing.push((packet.len() % 255) as u8);
    let mut data_at = 0;
    let mut continued = false;
    for chunk in lacing.chunks(255) {
        let bytes: usize = chunk.iter().map(|&s| usize::from(s)).sum();
        let flags = if continued { 1 } else { first_flags };
        ogg_page(out, flags, granule, *seq, chunk, &packet[data_at..data_at + bytes]);
        data_at += bytes;
        *seq += 1;
        continued = true;
    }
}

/// An Ogg Opus (or Vorbis) stream: identification header, the comments
/// (followed by `comment_padding` zero bytes, to span pages), and one audio
/// page whose granule position is `final_granule`.
pub fn ogg_stream(
    opus: bool,
    comments: &[u8],
    final_granule: u64,
    comment_padding: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 0;
    let (id, mut tags) = if opus {
        let mut id = b"OpusHead".to_vec();
        id.extend_from_slice(&[1, 2]);
        id.extend_from_slice(&312_u16.to_le_bytes());
        id.extend_from_slice(&48_000_u32.to_le_bytes());
        id.extend_from_slice(&[0, 0, 0]);
        (id, b"OpusTags".to_vec())
    } else {
        let mut id = b"\x01vorbis".to_vec();
        id.extend_from_slice(&0_u32.to_le_bytes());
        id.push(2);
        id.extend_from_slice(&44_100_u32.to_le_bytes());
        id.extend_from_slice(&[0; 12]);
        id.extend_from_slice(&[0xb8, 1]);
        (id, b"\x03vorbis".to_vec())
    };
    tags.extend_from_slice(comments);
    tags.extend(std::iter::repeat_n(0_u8, comment_padding));
    ogg_packet(&mut out, &id, 2, 0, &mut seq);
    ogg_packet(&mut out, &tags, 0, 0, &mut seq);
    ogg_packet(&mut out, &[0; 100], 4, final_granule, &mut seq);
    out
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn atoms(kind: &[u8; 4], parts: &[Vec<u8>]) -> Vec<u8> {
    atom(kind, &parts.concat())
}

/// One `ilst` item.
#[derive(Clone, Debug)]
pub struct Mp4Item(Vec<u8>);

impl Mp4Item {
    fn with_data(kind: &[u8; 4], type_code: u32, value: &[u8]) -> Self {
        let mut data = type_code.to_be_bytes().to_vec();
        data.extend_from_slice(&[0; 4]);
        data.extend_from_slice(value);
        Self(atom(kind, &atom(b"data", &data)))
    }

    pub fn text(kind: &[u8; 4], value: &str) -> Self {
        Self::with_data(kind, 1, value.as_bytes())
    }

    pub fn pair(kind: &[u8; 4], n: u16, total: u16) -> Self {
        let mut v = vec![0, 0];
        v.extend_from_slice(&n.to_be_bytes());
        v.extend_from_slice(&total.to_be_bytes());
        v.extend_from_slice(&[0, 0]);
        Self::with_data(kind, 0, &v)
    }

    /// `gnre`: an ID3v1 genre index plus one.
    pub fn genre(id3_index_plus_one: u16) -> Self {
        Self::with_data(b"gnre", 0, &id3_index_plus_one.to_be_bytes())
    }

    pub fn flag(kind: &[u8; 4], on: bool) -> Self {
        Self::with_data(kind, 21, &[u8::from(on)])
    }

    pub fn cover(type_code: u32, image: &[u8]) -> Self {
        Self::with_data(b"covr", type_code, image)
    }
}

/// An MP4 audio file: `ftyp`, `moov` (before or after `mdat`), and 4096
/// bytes of `mdat`. `duration` is in samples at `rate`.
pub fn mp4(
    items: &[Mp4Item],
    rate: u32,
    duration: u32,
    codec: &[u8; 4],
    bitrate: u32,
    moov_last: bool,
) -> Vec<u8> {
    let mut times = vec![0_u8; 12];
    times.extend_from_slice(&rate.to_be_bytes());
    times.extend_from_slice(&duration.to_be_bytes());
    let mvhd = atom(b"mvhd", &[times.clone(), vec![0; 80]].concat());
    let mdhd = atom(b"mdhd", &[times, vec![0; 4]].concat());
    let mut hdlr = vec![0; 8];
    hdlr.extend_from_slice(b"soun");
    hdlr.extend_from_slice(&[0; 13]);
    let hdlr = atom(b"hdlr", &hdlr);
    let mut entry = vec![0_u8; 6];
    entry.extend_from_slice(&1_u16.to_be_bytes());
    entry.extend_from_slice(&[0; 8]);
    entry.extend_from_slice(&2_u16.to_be_bytes());
    entry.extend_from_slice(&16_u16.to_be_bytes());
    entry.extend_from_slice(&[0; 4]);
    entry.extend_from_slice(&(rate << 16).to_be_bytes());
    if codec == b"mp4a" {
        let mut dc = vec![0x40, 0x15, 0, 0, 0];
        dc.extend_from_slice(&bitrate.to_be_bytes());
        dc.extend_from_slice(&bitrate.to_be_bytes());
        let mut es = vec![0, 1, 0, 0x04, dc.len() as u8];
        es.extend(dc);
        let mut esds = vec![0; 4];
        esds.extend_from_slice(&[0x03, es.len() as u8]);
        esds.extend(es);
        entry.extend(atom(b"esds", &esds));
    } else {
        let mut cfg = vec![0; 4];
        cfg.extend_from_slice(&4096_u32.to_be_bytes());
        cfg.extend_from_slice(&[0, 16, 40, 10, 14, 2, 0, 255]);
        cfg.extend_from_slice(&[0; 12]);
        entry.extend(atom(b"alac", &cfg));
    }
    let mut stsd = vec![0; 4];
    stsd.extend_from_slice(&1_u32.to_be_bytes());
    stsd.extend(atom(codec, &entry));
    let stbl = atoms(b"stbl", &[atom(b"stsd", &stsd)]);
    let minf = atoms(b"minf", &[stbl]);
    let mdia = atoms(b"mdia", &[mdhd, hdlr.clone(), minf]);
    let trak = atoms(b"trak", &[mdia]);
    let ilst = atom(b"ilst", &items.iter().map(|i| i.0.clone()).collect::<Vec<_>>().concat());
    let meta = atom(b"meta", &[vec![0; 4], hdlr, ilst].concat());
    let udta = atoms(b"udta", &[meta]);
    let moov = atoms(b"moov", &[mvhd, trak, udta]);
    let ftyp = atom(b"ftyp", b"M4A \0\0\0\0M4A mp42isom");
    let mdat = atom(b"mdat", &[0; 4096]);
    if moov_last { [ftyp, mdat, moov].concat() } else { [ftyp, moov, mdat].concat() }
}

/// A WAV file with `data_len` bytes of silence, an optional `LIST/INFO`,
/// and an optional `id3 ` chunk.
pub fn wav(
    rate: u32,
    channels: u16,
    bits: u16,
    data_len: u32,
    info: &[(&str, &str)],
    id3: Option<&[u8]>,
) -> Vec<u8> {
    let chunk = |id: &[u8; 4], body: &[u8]| {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u32).to_le_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    };
    let align = channels * bits / 8;
    let mut fmt = 1_u16.to_le_bytes().to_vec();
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    fmt.extend_from_slice(&align.to_le_bytes());
    fmt.extend_from_slice(&bits.to_le_bytes());
    let mut body = b"WAVE".to_vec();
    body.extend(chunk(b"fmt ", &fmt));
    if !info.is_empty() {
        let mut list = b"INFO".to_vec();
        for (id, value) in info {
            let mut v = value.as_bytes().to_vec();
            v.push(0);
            list.extend(chunk(id.as_bytes().try_into().unwrap(), &v));
        }
        body.extend(chunk(b"LIST", &list));
    }
    if let Some(tag) = id3 {
        body.extend(chunk(b"id3 ", tag));
    }
    body.extend(chunk(b"data", &vec![0; data_len as usize]));
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

/// A PNG signature and IHDR chunk: enough to sniff and size, not to decode.
pub fn tiny_png(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    out.extend_from_slice(&13_u32.to_be_bytes());
    out.extend_from_slice(b"IHDR");
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
    out
}

/// A JPEG with an APP0 and a baseline SOF0 segment.
pub fn tiny_jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut out = vec![0xff, 0xd8, 0xff, 0xe0, 0, 16];
    out.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    out.extend_from_slice(&[0xff, 0xc0, 0, 17, 8]);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1, 0xff, 0xd9]);
    out
}
