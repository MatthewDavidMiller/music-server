//! Whole files sealed with the master key: the account roster and each
//! user's playlists, stars, and play counts.
//!
//! A sealed file is `"MSF1" || nonce || ciphertext || tag`, encrypted with
//! AES-256-GCM under a key derived for this purpose only. The authenticated
//! label names the file (`users`, `user/alice`), so a file copied over
//! another one fails to open instead of being believed.

use ms_common::{Error, Result};
use ms_crypto::{MasterKey, SecretBytes, SubKey};

const MAGIC: &[u8; 4] = b"MSF1";
/// What sealing adds to a file: the magic, the nonce, and the tag.
pub const OVERHEAD: usize = 4 + 12 + 16;
const PURPOSE: &str = "files/v1";

#[derive(Debug)]
pub struct Sealer {
    key: SubKey,
}

impl Sealer {
    pub fn new(master: &MasterKey) -> Result<Self> {
        let key = master.derive(PURPOSE).map_err(|_| Error::new("key derivation failed"))?;
        Ok(Self { key })
    }

    fn aad(label: &str) -> String {
        format!("music-server/v1/file/{label}")
    }

    pub fn is_sealed(raw: &[u8]) -> bool {
        raw.starts_with(MAGIC)
    }

    pub fn seal(&self, label: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        let sealed = ms_crypto::seal(self.key.bytes(), Self::aad(label).as_bytes(), plaintext)
            .map_err(|_| Error::new("encryption failed"))?;
        let mut out = Vec::with_capacity(MAGIC.len() + sealed.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Opens a sealed file. A plain file, a file sealed for another label,
    /// or one sealed under another key all fail.
    pub fn open(&self, label: &str, raw: &[u8]) -> Result<SecretBytes> {
        let sealed = raw.strip_prefix(MAGIC.as_slice()).ok_or_else(|| Error::new("not sealed"))?;
        ms_crypto::open(self.key.bytes(), Self::aad(label).as_bytes(), sealed)
            .map_err(|_| Error::new(format!("{label} failed authentication")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealer(c: u8) -> Sealer {
        Sealer::new(&MasterKey::from_hex(&[c; 64]).unwrap()).unwrap()
    }

    #[test]
    fn files_round_trip_and_are_bound_to_label_and_key() {
        let s = sealer(b'1');
        let sealed = s.seal("user/alice", b"{\"status\":\"ok\"}").unwrap();
        assert!(!sealed.windows(6).any(|w| w == b"status"));
        assert_eq!(s.open("user/alice", &sealed).unwrap().expose(), b"{\"status\":\"ok\"}");
        assert!(s.open("user/bob", &sealed).is_err());
        assert!(sealer(b'2').open("user/alice", &sealed).is_err());
        let mut flipped = sealed.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(s.open("user/alice", &flipped).is_err());
    }
}
