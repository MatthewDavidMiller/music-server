//! Status page sessions and CSRF tokens.
//!
//! Sessions are random 256-bit identifiers held only in memory, keyed by
//! HMAC(web key, id), so a restart signs everyone out and nothing on disk
//! yields a usable cookie. CSRF tokens are HMAC(web key, "csrf", session id);
//! the login form's token is bound to a pre-session cookie the same way.
//! Every comparison is constant-time.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ms_common::{Error, Result, rand};
use ms_crypto::{SubKey, ct_equal, hex, hmac_sha256};
use ms_store::lock;

pub const SESSION_COOKIE: &str = "ms_session";
pub const LOGIN_COOKIE: &str = "ms_login";
const MAXIMUM_SESSIONS: usize = 64;

#[derive(Debug)]
struct Session {
    expires: Instant,
    user: String,
}

#[derive(Debug)]
pub struct Sessions {
    web_key: SubKey,
    lifetime: Duration,
    live: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn new(web_key: SubKey, lifetime_s: u64) -> Self {
        Self {
            web_key,
            lifetime: Duration::from_secs(lifetime_s),
            live: Mutex::new(HashMap::new()),
        }
    }

    fn keyed(&self, label: &[u8], data: &str) -> String {
        let mut msg = Vec::with_capacity(label.len() + 1 + data.len());
        msg.extend_from_slice(label);
        msg.push(0);
        msg.extend_from_slice(data.as_bytes());
        // HMAC over a few hundred bytes cannot fail short of libcrypto
        // itself being broken; an empty key then matches nothing.
        hmac_sha256(self.web_key.bytes(), &msg).map(|d| hex(&d)).unwrap_or_default()
    }

    fn session_key(&self, id: &str) -> Option<String> {
        (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| self.keyed(b"session", id))
            .filter(|k| !k.is_empty())
    }

    /// Starts a session for `user` and returns its identifier.
    pub fn create(&self, user: &str) -> Result<String> {
        let id = rand::random_hex(32)?;
        let key =
            self.session_key(&id).ok_or_else(|| Error::new("unable to derive a session key"))?;
        let now = Instant::now();
        let mut sessions = lock(&self.live);
        sessions.retain(|_, s| s.expires > now);
        while sessions.len() >= MAXIMUM_SESSIONS {
            let oldest = sessions.iter().min_by_key(|(_, s)| s.expires).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => sessions.remove(&k),
                None => break,
            };
        }
        sessions.insert(key, Session { expires: now + self.lifetime, user: user.to_owned() });
        Ok(id)
    }

    /// The signed-in user of a live session.
    pub fn user(&self, id: &str) -> Option<String> {
        let key = self.session_key(id)?;
        lock(&self.live).get(&key).filter(|s| s.expires > Instant::now()).map(|s| s.user.clone())
    }

    pub fn end(&self, id: &str) {
        if let Some(key) = self.session_key(id) {
            lock(&self.live).remove(&key);
        }
    }

    pub fn csrf_token(&self, session_id: &str) -> String {
        self.keyed(b"csrf", session_id)
    }

    pub fn verify_csrf(&self, session_id: &str, token: &str) -> bool {
        let expected = self.csrf_token(session_id);
        !expected.is_empty() && ct_equal(expected.as_bytes(), token.as_bytes())
    }

    pub fn login_token(&self, nonce: &str) -> String {
        self.keyed(b"login", nonce)
    }

    pub fn verify_login_token(&self, nonce: &str, token: &str) -> bool {
        nonce.len() == 32 && ct_equal(self.login_token(nonce).as_bytes(), token.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_crypto::MasterKey;

    #[test]
    fn sessions_and_tokens() {
        let s =
            Sessions::new(MasterKey::from_hex(&[b'a'; 64]).unwrap().derive("web").unwrap(), 3600);
        let id = s.create("alice").unwrap();
        assert_eq!(s.user(&id).as_deref(), Some("alice"));
        assert_eq!(s.user("nothex"), None);
        let t = s.csrf_token(&id);
        assert!(s.verify_csrf(&id, &t));
        assert!(!s.verify_csrf(&id, "00"));
        let other = s.create("bob").unwrap();
        assert!(!s.verify_csrf(&other, &t));
        s.end(&id);
        assert_eq!(s.user(&id), None);
        let nonce = "0123456789abcdef0123456789abcdef";
        assert!(s.verify_login_token(nonce, &s.login_token(nonce)));
        assert!(!s.verify_login_token("short", &s.login_token("short")));
    }
}
