//! Subsonic authentication.
//!
//! Every call carries `u` and either `t` and `s` (the token,
//! `md5(password || salt)` in hex, with the client's salt) or `p` (the
//! password, plain or as `enc:` followed by its hex). Both need the
//! password itself on the server side, which is why the passwords stay
//! loaded from the users file rather than hashed.
//!
//! Comparisons are constant-time, and an unknown user is checked against a
//! dummy password so the answer takes as long as for a real one.

use std::collections::HashMap;

use ms_crypto::{SecretBytes, ct_equal, md5};
use ms_http::Form;
use ms_store::UserEntry;

use crate::error::{ApiError, ApiResult};

/// A signed-in caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub admin: bool,
}

#[derive(Debug)]
struct Account {
    password: SecretBytes,
    admin: bool,
}

/// The accounts the users file names, with their passwords.
#[derive(Debug)]
pub struct Credentials {
    accounts: HashMap<String, Account>,
    dummy: SecretBytes,
}

impl Credentials {
    pub fn new(entries: Vec<UserEntry>) -> Self {
        let accounts = entries
            .into_iter()
            .map(|e| (e.name, Account { password: e.password, admin: e.admin }))
            .collect();
        Self { accounts, dummy: SecretBytes::new(b"music-server-dummy-password".to_vec()) }
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn is_admin(&self, name: &str) -> bool {
        self.accounts.get(name).is_some_and(|a| a.admin)
    }

    /// Checks a password directly (the status page's login form).
    pub fn check_password(&self, name: &str, password: &str) -> Option<User> {
        let (secret, account) = match self.accounts.get(name) {
            Some(a) => (&a.password, Some(a)),
            None => (&self.dummy, None),
        };
        let ok = ct_equal(secret.expose(), password.as_bytes());
        account.filter(|_| ok).map(|a| User { name: name.to_owned(), admin: a.admin })
    }

    /// Authenticates a Subsonic call from its parameters.
    pub fn authenticate(&self, params: &Form, allow_password: bool) -> ApiResult<User> {
        if params.get("apiKey").is_some() {
            return Err(ApiError::new(42, "API keys are not supported; use a token or password"));
        }
        let name =
            params.get("u").filter(|u| !u.is_empty()).ok_or_else(|| ApiError::missing("u"))?;
        let (token, salt, password) = (params.get("t"), params.get("s"), params.get("p"));
        let (secret, account) = match self.accounts.get(name) {
            Some(a) => (&a.password, Some(a)),
            None => (&self.dummy, None),
        };
        let ok = match (token, salt, password) {
            (Some(_), _, Some(_)) => {
                return Err(ApiError::new(
                    43,
                    "Multiple conflicting authentication mechanisms provided",
                ));
            }
            (Some(t), Some(s), None) => {
                if t.len() != 32 || s.is_empty() || s.len() > 64 {
                    false
                } else {
                    let mut input = Vec::with_capacity(secret.len() + s.len());
                    input.extend_from_slice(secret.expose());
                    input.extend_from_slice(s.as_bytes());
                    let digest = md5(&input).map(|d| ms_crypto::hex(&d)).unwrap_or_default();
                    ms_crypto::cleanse(&mut input);
                    ct_equal(digest.as_bytes(), t.to_ascii_lowercase().as_bytes())
                }
            }
            (Some(_), None, None) => return Err(ApiError::missing("s")),
            (None, _, Some(p)) => {
                if !allow_password {
                    return Err(ApiError::new(
                        42,
                        "Password authentication is disabled; use a token",
                    ));
                }
                match p.strip_prefix("enc:") {
                    Some(hex) => ms_common::buf::hex_decode(hex)
                        .is_some_and(|raw| ct_equal(secret.expose(), &raw)),
                    None => ct_equal(secret.expose(), p.as_bytes()),
                }
            }
            (None, _, None) => return Err(ApiError::missing("t")),
        };
        match account {
            Some(a) if ok => Ok(User { name: name.to_owned(), admin: a.admin }),
            _ => Err(ApiError::wrong_credentials()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Credentials {
        Credentials::new(
            ms_store::users::parse_users_file(
                br#"{"version":1,"users":[{"name":"admin","password":"sesame12","admin":true},{"name":"bob","password":"hunter22"}]}"#,
            )
            .unwrap(),
        )
    }

    fn form(pairs: &[(&str, &str)]) -> Form {
        Form::new(pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect())
    }

    fn token(password: &str, salt: &str) -> String {
        ms_crypto::hex(&md5(format!("{password}{salt}").as_bytes()).unwrap())
    }

    #[test]
    fn token_auth() {
        let c = creds();
        let t = token("sesame12", "c19b2d");
        let user =
            c.authenticate(&form(&[("u", "admin"), ("t", &t), ("s", "c19b2d")]), true).unwrap();
        assert!(user.admin);
        let upper = t.to_ascii_uppercase();
        assert!(
            c.authenticate(&form(&[("u", "admin"), ("t", &upper), ("s", "c19b2d")]), true).is_ok()
        );
        assert_eq!(
            c.authenticate(&form(&[("u", "admin"), ("t", &t), ("s", "other")]), true)
                .unwrap_err()
                .code,
            40
        );
        assert_eq!(
            c.authenticate(&form(&[("u", "nobody"), ("t", &t), ("s", "c19b2d")]), true)
                .unwrap_err()
                .code,
            40
        );
        assert_eq!(c.authenticate(&form(&[("u", "admin"), ("t", &t)]), true).unwrap_err().code, 10);
    }

    #[test]
    fn password_auth_and_conflicts() {
        let c = creds();
        assert!(!c.authenticate(&form(&[("u", "bob"), ("p", "hunter22")]), true).unwrap().admin);
        let enc = format!("enc:{}", ms_crypto::hex(b"hunter22"));
        assert!(c.authenticate(&form(&[("u", "bob"), ("p", &enc)]), true).is_ok());
        assert_eq!(
            c.authenticate(&form(&[("u", "bob"), ("p", "enc:zz")]), true).unwrap_err().code,
            40
        );
        assert_eq!(
            c.authenticate(&form(&[("u", "bob"), ("p", "hunter22")]), false).unwrap_err().code,
            42
        );
        assert_eq!(
            c.authenticate(&form(&[("u", "bob"), ("p", "x"), ("t", "y")]), true).unwrap_err().code,
            43
        );
        assert_eq!(c.authenticate(&form(&[("p", "hunter22")]), true).unwrap_err().code, 10);
        assert_eq!(c.authenticate(&form(&[("u", "bob")]), true).unwrap_err().code, 10);
        assert_eq!(
            c.authenticate(&form(&[("u", "bob"), ("apiKey", "k")]), true).unwrap_err().code,
            42
        );
        assert!(c.check_password("admin", "sesame12").is_some());
        assert!(c.check_password("admin", "sesame13").is_none());
        assert!(c.check_password("ghost", "music-server-dummy-password").is_none());
    }
}
