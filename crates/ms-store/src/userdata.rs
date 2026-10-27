//! What each user owns: playlists, starred items, and play counts.
//!
//! Entries refer to library items by their stable identifiers (as text). A
//! track that disappears from the library is not removed from a playlist; it
//! is skipped when the playlist is shown, and comes back if the file does.

use std::collections::HashMap;

use ms_common::json::{self, Value};
use ms_common::{Error, Result};

use crate::jsonw::{self, Obj};

pub const MAX_PLAYLISTS: usize = 1000;
pub const MAX_PLAYLIST_ENTRIES: usize = 10_000;
pub const MAX_STARS: usize = 50_000;
pub const MAX_PLAYS: usize = 200_000;
pub const MAX_NAME: usize = 256;
pub const MAX_COMMENT: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub comment: String,
    pub public: bool,
    /// Unix milliseconds.
    pub created: i64,
    pub changed: i64,
    pub entries: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Star {
    pub id: String,
    /// Unix milliseconds.
    pub at: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Play {
    pub count: u64,
    /// Unix milliseconds of the latest play.
    pub last: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserData {
    pub playlists: Vec<Playlist>,
    pub starred: Vec<Star>,
    pub plays: HashMap<String, Play>,
}

/// Cuts `s` to at most `max` bytes on a character boundary.
pub fn truncate(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

impl UserData {
    pub fn playlist(&self, id: &str) -> Option<&Playlist> {
        self.playlists.iter().find(|p| p.id == id)
    }

    pub fn playlist_mut(&mut self, id: &str) -> Option<&mut Playlist> {
        self.playlists.iter_mut().find(|p| p.id == id)
    }

    pub fn starred_at(&self, id: &str) -> Option<i64> {
        self.starred.iter().find(|s| s.id == id).map(|s| s.at)
    }

    pub fn star(&mut self, id: &str, at: i64) -> Result<()> {
        if self.starred_at(id).is_some() {
            return Ok(());
        }
        if self.starred.len() >= MAX_STARS {
            return Err(Error::new("too many starred items"));
        }
        self.starred.push(Star { id: id.to_owned(), at });
        Ok(())
    }

    pub fn unstar(&mut self, id: &str) {
        self.starred.retain(|s| s.id != id);
    }

    /// Counts a play. Past the ceiling, the oldest play record is dropped.
    pub fn record_play(&mut self, id: &str, at: i64) {
        if !self.plays.contains_key(id)
            && self.plays.len() >= MAX_PLAYS
            && let Some(oldest) =
                self.plays.iter().min_by_key(|(_, p)| p.last).map(|(k, _)| k.clone())
        {
            self.plays.remove(&oldest);
        }
        let p = self.plays.entry(id.to_owned()).or_default();
        p.count = p.count.saturating_add(1);
        p.last = p.last.max(at);
    }

    pub fn encode(&self) -> String {
        let mut out = String::from("{\"version\":1,\"playlists\":[");
        for (i, p) in self.playlists.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            Obj::new(&mut out)
                .str("id", &p.id)
                .str("name", &p.name)
                .str("comment", &p.comment)
                .bool("public", p.public)
                .int("created", p.created)
                .int("changed", p.changed)
                .strs("entries", &p.entries)
                .end();
        }
        out.push_str("],\"starred\":[");
        for (i, s) in self.starred.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            Obj::new(&mut out).str("id", &s.id).int("at", s.at).end();
        }
        out.push_str("],\"plays\":[");
        let mut plays: Vec<_> = self.plays.iter().collect();
        plays.sort_by(|a, b| a.0.cmp(b.0));
        for (i, (id, p)) in plays.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            Obj::new(&mut out)
                .str("id", id)
                .int("count", i64::try_from(p.count).unwrap_or(i64::MAX))
                .int("last", p.last)
                .end();
        }
        out.push_str("]}");
        out
    }

    pub fn decode(raw: &[u8]) -> Result<Self> {
        // User data can be large; allow it more than the default budget.
        let doc =
            json::parse_with_budget(raw, raw.len().saturating_mul(96).saturating_add(1 << 20))
                .map_err(|e| Error::new(format!("user data is not valid JSON: {e}")))?;
        let list = |name: &str| doc.get(name).and_then(Value::as_array).unwrap_or_default();
        let mut data = UserData::default();
        for p in list("playlists").iter().take(MAX_PLAYLISTS) {
            let (Some(id), Some(name)) = (jsonw::str(p, "id"), jsonw::str(p, "name")) else {
                continue;
            };
            let mut entries = jsonw::strs(p, "entries").unwrap_or_default();
            entries.truncate(MAX_PLAYLIST_ENTRIES);
            data.playlists.push(Playlist {
                id,
                name,
                comment: jsonw::str(p, "comment").unwrap_or_default(),
                public: jsonw::bool(p, "public").unwrap_or(false),
                created: jsonw::int(p, "created").unwrap_or(0),
                changed: jsonw::int(p, "changed").unwrap_or(0),
                entries,
            });
        }
        for s in list("starred").iter().take(MAX_STARS) {
            if let Some(id) = jsonw::str(s, "id") {
                data.starred.push(Star { id, at: jsonw::int(s, "at").unwrap_or(0) });
            }
        }
        for p in list("plays").iter().take(MAX_PLAYS) {
            if let Some(id) = jsonw::str(p, "id") {
                let count = jsonw::int(p, "count").and_then(|c| u64::try_from(c).ok()).unwrap_or(0);
                data.plays.insert(id, Play { count, last: jsonw::int(p, "last").unwrap_or(0) });
            }
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_operations() {
        let mut d = UserData::default();
        d.playlists.push(Playlist {
            id: "pl-1".into(),
            name: "Road \"trip\"".into(),
            comment: "é".into(),
            public: true,
            created: 1,
            changed: 2,
            entries: vec!["tr-a".into(), "tr-b".into(), "tr-a".into()],
        });
        d.star("al-x", 5).unwrap();
        d.star("al-x", 6).unwrap();
        d.record_play("tr-a", 10);
        d.record_play("tr-a", 7);
        assert_eq!(d.plays["tr-a"], Play { count: 2, last: 10 });
        assert_eq!(d.starred_at("al-x"), Some(5));
        let back = UserData::decode(d.encode().as_bytes()).unwrap();
        assert_eq!(back, d);
        d.unstar("al-x");
        assert!(d.starred.is_empty());
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(UserData::decode(b"{}").unwrap(), UserData::default());
        assert!(UserData::decode(b"[").is_err());
    }
}
