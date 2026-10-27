//! Accounts. Ansible owns the list: a JSON users file, mounted as a Podman
//! secret, names every account with its password and admin flag:
//!
//! ```json
//! {"version": 1, "users": [{"name": "alice", "password": "...", "admin": true}]}
//! ```
//!
//! The passwords stay in memory only. Subsonic's token authentication needs
//! the plain password on the server side, and the secret is already where
//! that lives; a copy on disk, even sealed, would only be one more place to
//! protect.
//!
//! The roster, sealed in the data directory, remembers every account ever
//! seen. Each start syncs it to the file: new names are added, admin flags
//! follow the file, an account missing from the file is disabled (its
//! playlists and stars kept), and one that reappears is enabled again.

use std::fmt;

use ms_common::json::{self, Value};
use ms_common::{Error, Result};
use ms_crypto::SecretBytes;

use crate::jsonw::{self, Obj};

pub const MAX_USERS: usize = 50;
pub const MAX_USERS_FILE: usize = 64 * 1024;
pub const MIN_PASSWORD: usize = 8;
pub const MAX_PASSWORD: usize = 128;

/// An account as the users file states it.
pub struct UserEntry {
    pub name: String,
    pub password: SecretBytes,
    pub admin: bool,
}

impl fmt::Debug for UserEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserEntry")
            .field("name", &self.name)
            .field("admin", &self.admin)
            .finish_non_exhaustive()
    }
}

pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Parses and validates the users file. Messages name the entry that is
/// wrong, never a password.
pub fn parse_users_file(raw: &[u8]) -> Result<Vec<UserEntry>> {
    let mut doc = json::parse(raw)
        .map_err(|e| Error::new(format!("the users file is not valid JSON: {e}")))?;
    let result = (|| {
        if !doc.only_keys(&["version", "users"])
            || doc.get("version").and_then(Value::as_u64) != Some(1)
        {
            return Err(Error::new("the users file must be {\"version\": 1, \"users\": [...]}"));
        }
        let list = doc
            .get("users")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::new("the users file has no users list"))?;
        if list.is_empty() || list.len() > MAX_USERS {
            return Err(Error::new(format!("the users file must list 1-{MAX_USERS} users")));
        }
        let mut out: Vec<UserEntry> = Vec::with_capacity(list.len());
        for (i, u) in list.iter().enumerate() {
            let n = i + 1;
            if !u.only_keys(&["name", "password", "admin"]) {
                return Err(Error::new(format!("user {n} has an unknown field")));
            }
            let name = u.get("name").and_then(Value::as_str).unwrap_or("");
            if !valid_name(name) {
                return Err(Error::new(format!(
                    "user {n}: the name must be 1-64 of A-Z a-z 0-9 . _ -"
                )));
            }
            if out.iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
                return Err(Error::new(format!("user {name} is listed twice")));
            }
            let password = u.get("password").and_then(Value::as_str).unwrap_or("");
            if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&password.len())
                || password.chars().any(char::is_control)
            {
                return Err(Error::new(format!(
                    "user {name}: the password must be {MIN_PASSWORD}-{MAX_PASSWORD} bytes without control characters"
                )));
            }
            let admin = match u.get("admin") {
                None => false,
                Some(v) => v.as_bool().ok_or_else(|| {
                    Error::new(format!("user {name}: admin must be true or false"))
                })?,
            };
            out.push(UserEntry {
                name: name.to_owned(),
                password: SecretBytes::new(password.as_bytes().to_vec()),
                admin,
            });
        }
        if !out.iter().any(|u| u.admin) {
            return Err(Error::new("the users file must make at least one user an admin"));
        }
        Ok(out)
    })();
    doc.wipe();
    result
}

/// An account the server has seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RosterUser {
    pub name: String,
    pub admin: bool,
    pub enabled: bool,
    pub created: i64,
    pub disabled_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub added: Vec<String>,
    pub disabled: Vec<String>,
    pub enabled: Vec<String>,
    pub admin_changed: Vec<String>,
}

impl SyncReport {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.disabled.is_empty()
            && self.enabled.is_empty()
            && self.admin_changed.is_empty()
    }
}

/// Brings `roster` in line with the users file.
pub fn sync(roster: &mut Vec<RosterUser>, entries: &[UserEntry], now: i64) -> SyncReport {
    let mut report = SyncReport::default();
    for e in entries {
        if let Some(r) = roster.iter_mut().find(|r| r.name == e.name) {
            if !r.enabled {
                r.enabled = true;
                r.disabled_at = 0;
                report.enabled.push(e.name.clone());
            }
            if r.admin != e.admin {
                r.admin = e.admin;
                report.admin_changed.push(e.name.clone());
            }
        } else {
            roster.push(RosterUser {
                name: e.name.clone(),
                admin: e.admin,
                enabled: true,
                created: now,
                disabled_at: 0,
            });
            report.added.push(e.name.clone());
        }
    }
    for r in roster.iter_mut() {
        if r.enabled && !entries.iter().any(|e| e.name == r.name) {
            r.enabled = false;
            r.admin = false;
            r.disabled_at = now;
            report.disabled.push(r.name.clone());
        }
    }
    roster.sort_by(|a, b| a.name.cmp(&b.name));
    report
}

pub fn encode_roster(roster: &[RosterUser]) -> String {
    let mut out = String::from("{\"version\":1,\"users\":[");
    for (i, r) in roster.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        Obj::new(&mut out)
            .str("name", &r.name)
            .bool("admin", r.admin)
            .bool("enabled", r.enabled)
            .int("created", r.created)
            .int("disabled_at", r.disabled_at)
            .end();
    }
    out.push_str("]}");
    out
}

pub fn decode_roster(raw: &[u8]) -> Result<Vec<RosterUser>> {
    let doc =
        json::parse(raw).map_err(|e| Error::new(format!("the roster is not valid JSON: {e}")))?;
    let list = doc
        .get("users")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new("the roster has no users list"))?;
    list.iter()
        .map(|u| {
            Ok(RosterUser {
                name: jsonw::str(u, "name")
                    .filter(|n| valid_name(n))
                    .ok_or_else(|| Error::new("a roster entry has no valid name"))?,
                admin: jsonw::bool(u, "admin").unwrap_or(false),
                enabled: jsonw::bool(u, "enabled").unwrap_or(false),
                created: jsonw::int(u, "created").unwrap_or(0),
                disabled_at: jsonw::int(u, "disabled_at").unwrap_or(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(json: &str) -> Result<Vec<UserEntry>> {
        parse_users_file(json.as_bytes())
    }

    #[test]
    fn users_file_validation() {
        let ok = entries(r#"{"version":1,"users":[{"name":"alice","password":"correct horse","admin":true},{"name":"bob.b","password":"12345678"}]}"#).unwrap();
        assert_eq!(ok.len(), 2);
        assert!(ok[0].admin && !ok[1].admin);
        assert_eq!(ok[1].password.expose(), b"12345678");
        assert!(!format!("{:?}", ok[0]).contains("horse"));
        for (bad, why) in [
            (r#"{"version":2,"users":[]}"#, "version"),
            (r#"{"version":1,"users":[]}"#, "1-50"),
            (
                r#"{"version":1,"users":[{"name":"a b","password":"12345678","admin":true}]}"#,
                "name",
            ),
            (r#"{"version":1,"users":[{"name":"a","password":"short","admin":true}]}"#, "password"),
            (r#"{"version":1,"users":[{"name":"a","password":"12345678"}]}"#, "admin"),
            (
                r#"{"version":1,"users":[{"name":"a","password":"12345678","admin":true},{"name":"A","password":"12345678"}]}"#,
                "twice",
            ),
            (
                r#"{"version":1,"users":[{"name":"a","password":"12345678","admin":"yes"}]}"#,
                "true or false",
            ),
            (
                r#"{"version":1,"users":[{"name":"a","password":"12345678","admin":true,"x":1}]}"#,
                "unknown",
            ),
            ("not json", "JSON"),
        ] {
            let e = entries(bad).unwrap_err();
            assert!(e.message().contains(why), "{bad}: {e}");
            assert!(!e.message().contains("12345678"));
        }
    }

    #[test]
    fn roster_sync() {
        let file = |json: &str| entries(json).unwrap();
        let mut roster = Vec::new();
        let r = sync(
            &mut roster,
            &file(
                r#"{"version":1,"users":[{"name":"a","password":"12345678","admin":true},{"name":"b","password":"12345678"}]}"#,
            ),
            10,
        );
        assert_eq!(r.added, ["a", "b"]);
        let r = sync(
            &mut roster,
            &file(r#"{"version":1,"users":[{"name":"a","password":"12345678","admin":true}]}"#),
            20,
        );
        assert_eq!(r.disabled, ["b"]);
        assert!(!roster[1].enabled && roster[1].disabled_at == 20);
        let r = sync(
            &mut roster,
            &file(
                r#"{"version":1,"users":[{"name":"a","password":"12345678"},{"name":"b","password":"12345678","admin":true}]}"#,
            ),
            30,
        );
        assert_eq!(r.enabled, ["b"]);
        assert_eq!(r.admin_changed, ["a", "b"]);
        let again = sync(
            &mut roster,
            &file(
                r#"{"version":1,"users":[{"name":"a","password":"12345678"},{"name":"b","password":"12345678","admin":true}]}"#,
            ),
            40,
        );
        assert!(again.is_empty());
        assert_eq!(decode_roster(encode_roster(&roster).as_bytes()).unwrap(), roster);
    }
}
