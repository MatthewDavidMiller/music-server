//! Stable identifiers for library entities.
//!
//! An identifier is a kind prefix and the first 96 bits of a SHA-256 over a
//! domain-separated description of the entity: a track by its path relative
//! to the library root, an album by its grouping key, an artist by its
//! folded name, a directory by its path. Nothing secret goes in, so the
//! same library always yields the same identifiers, across rescans,
//! restarts, rebuilt indexes, and master-key rotation. A phone's offline
//! cache keeps matching after any of those.
//!
//! Playlists are not derived from anything; they get random identifiers.

use std::fmt;

use ms_common::{Error, Result};

pub const ID_BYTES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Track,
    Album,
    Artist,
    Dir,
    Playlist,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Track => "tr",
            Kind::Album => "al",
            Kind::Artist => "ar",
            Kind::Dir => "dr",
            Kind::Playlist => "pl",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id {
    pub kind: Kind,
    pub bytes: [u8; ID_BYTES],
}

impl Id {
    /// The identifier of `parts` under `kind`: SHA-256 of a domain label and
    /// each part, NUL-separated.
    pub fn derive(kind: Kind, parts: &[&[u8]]) -> Self {
        let mut input = Vec::with_capacity(64);
        input.extend_from_slice(b"music-server/");
        input.extend_from_slice(kind.prefix().as_bytes());
        for p in parts {
            input.push(0);
            input.extend_from_slice(p);
        }
        // SHA-256 over a short buffer fails only if libcrypto itself is
        // broken; the fallback still yields a fixed (if shared) identifier.
        let digest = ms_crypto::sha256(&input).unwrap_or([0; 32]);
        let mut bytes = [0; ID_BYTES];
        bytes.copy_from_slice(&digest[..ID_BYTES]);
        Self { kind, bytes }
    }

    pub fn random(kind: Kind) -> Result<Self> {
        let bytes = ms_common::rand::random_array()?;
        Ok(Self { kind, bytes })
    }

    /// Parses `tr-<24 lowercase hex>` and the other prefixes, strictly.
    pub fn parse(text: &str) -> Result<Self> {
        let (prefix, hex) = text.split_once('-').ok_or_else(|| Error::new("malformed id"))?;
        let kind = match prefix {
            "tr" => Kind::Track,
            "al" => Kind::Album,
            "ar" => Kind::Artist,
            "dr" => Kind::Dir,
            "pl" => Kind::Playlist,
            _ => return Err(Error::new("malformed id")),
        };
        if hex.len() != ID_BYTES * 2
            || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::new("malformed id"));
        }
        let decoded = ms_common::buf::hex_decode(hex).ok_or_else(|| Error::new("malformed id"))?;
        let mut bytes = [0; ID_BYTES];
        bytes.copy_from_slice(&decoded);
        Ok(Self { kind, bytes })
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.kind.prefix(), ms_common::buf::hex(&self.bytes))
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_stable_and_domain_separated() {
        let a = Id::derive(Kind::Track, &[b"Artist/Album/01.flac"]);
        assert_eq!(a, Id::derive(Kind::Track, &[b"Artist/Album/01.flac"]));
        assert_eq!(a.to_string().len(), 27);
        assert_ne!(a.bytes, Id::derive(Kind::Dir, &[b"Artist/Album/01.flac"]).bytes);
        assert_ne!(
            Id::derive(Kind::Album, &[b"ab", b"c"]),
            Id::derive(Kind::Album, &[b"a", b"bc"])
        );
        assert_eq!(Id::parse(&a.to_string()).unwrap(), a);
    }

    #[test]
    fn golden_track_id() {
        // sha256("music-server/tr\0a.mp3")[..12], computed independently.
        assert_eq!(Id::derive(Kind::Track, &[b"a.mp3"]).to_string(), "tr-f2d25e9aea4acbac7c413385");
    }

    #[test]
    fn parsing_is_strict() {
        for bad in [
            "",
            "tr",
            "tr-",
            "xx-000000000000000000000000",
            "tr-00000000000000000000000",
            "tr-00000000000000000000000G",
            "tr-0000000000000000000000AA",
            "tr-0000000000000000000000000",
        ] {
            assert!(Id::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Id::parse("pl-00112233445566778899aabb").unwrap().kind, Kind::Playlist);
    }
}
