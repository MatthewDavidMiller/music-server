//! Playlists. Each belongs to one user and lives in that user's data; a
//! public playlist can be read, but not changed, by everyone else. Admins
//! may change or delete anyone's.

use ms_common::timefmt;
use ms_library::{Entity, Id, Kind};
use ms_store::userdata::{MAX_COMMENT, MAX_NAME, MAX_PLAYLIST_ENTRIES, MAX_PLAYLISTS, truncate};
use ms_store::{Playlist, lock};

use crate::encode::Node;
use crate::error::{ApiError, ApiResult};
use crate::params::Params;
use crate::{Api, Reply, User};

fn now_ms() -> i64 {
    timefmt::unix_ms()
}

impl Api {
    /// The enabled accounts, the caller first.
    fn owners_for(&self, user: &User) -> Vec<String> {
        let mut names = vec![user.name.clone()];
        names.extend(
            self.roster.iter().filter(|r| r.enabled && r.name != user.name).map(|r| r.name.clone()),
        );
        names
    }

    /// The playlist with `id` and its owner, if `user` may see it.
    pub(crate) fn find_playlist(&self, user: &User, id: &str) -> ApiResult<(String, Playlist)> {
        for owner in self.owners_for(user) {
            let cell = self.store.user(&owner)?;
            let data = lock(&cell);
            if let Some(p) = data.playlist(id) {
                if owner == user.name || p.public || user.admin {
                    return Ok((owner, p.clone()));
                }
                break;
            }
        }
        Err(ApiError::not_found("Playlist"))
    }

    /// Track ids from `songId`-style parameters, checked against the
    /// library.
    fn track_ids(&self, values: &[&str]) -> ApiResult<Vec<String>> {
        let lib = self.library.library();
        values
            .iter()
            .map(|v| match Id::parse(v) {
                Ok(id)
                    if id.kind == Kind::Track
                        && matches!(lib.lookup(&id), Some(Entity::Track(_))) =>
                {
                    Ok(id.to_string())
                }
                _ => Err(ApiError::not_found("Song")),
            })
            .collect()
    }

    fn playlist_reply(&self, user: &User, owner: &str, p: &Playlist) -> ApiResult<Reply> {
        self.with_view(user, |v| Ok(Reply::Node(v.playlist(owner, p, true))))
    }

    pub(crate) fn get_playlists(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let only = p.str("username").filter(|u| !u.is_empty());
        if only.is_some_and(|u| u != user.name) && !user.admin {
            return Err(ApiError::not_authorized());
        }
        let mut lists: Vec<(String, Playlist)> = Vec::new();
        for owner in self.owners_for(user) {
            if only.is_some_and(|u| u != owner) {
                continue;
            }
            let cell = self.store.user(&owner)?;
            let data = lock(&cell);
            for pl in &data.playlists {
                if owner == user.name || pl.public || only.is_some() {
                    lists.push((owner.clone(), pl.clone()));
                }
            }
        }
        self.with_view(user, |v| {
            Ok(Reply::Node(
                Node::new("playlists").list(
                    "playlist",
                    lists.iter().map(|(o, pl)| v.playlist(o, pl, false)).collect(),
                ),
            ))
        })
    }

    pub(crate) fn get_playlist(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let (owner, pl) = self.find_playlist(user, p.req("id")?)?;
        self.playlist_reply(user, &owner, &pl)
    }

    pub(crate) fn create_playlist(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let songs = self.track_ids(&p.all("songId"))?;
        if songs.len() > MAX_PLAYLIST_ENTRIES {
            return Err(ApiError::generic("Too many songs for one playlist"));
        }
        let now = now_ms();
        let (owner, playlist) = if let Some(id) = p.str("playlistId").filter(|s| !s.is_empty()) {
            // Replaces an existing playlist's songs (and optionally its name).
            let (owner, _) = self.find_playlist(user, id)?;
            if owner != user.name && !user.admin {
                return Err(ApiError::not_authorized());
            }
            let name = p.str("name").map(|n| truncate(n.trim(), MAX_NAME));
            let id = id.to_owned();
            let updated = self.store.update(&owner, |d| {
                let pl = d
                    .playlist_mut(&id)
                    .ok_or_else(|| ms_common::Error::new("Playlist not found"))?;
                pl.entries.clone_from(&songs);
                if let Some(n) = name.filter(|n| !n.is_empty()) {
                    pl.name = n;
                }
                pl.changed = now;
                Ok(pl.clone())
            })?;
            (owner, updated)
        } else {
            let name = truncate(p.req("name")?.trim(), MAX_NAME);
            if name.is_empty() {
                return Err(ApiError::missing("name"));
            }
            let pl = Playlist {
                id: Id::random(Kind::Playlist)?.to_string(),
                name,
                comment: String::new(),
                public: false,
                created: now,
                changed: now,
                entries: songs,
            };
            let made = pl.clone();
            self.store.update(&user.name, |d| {
                if d.playlists.len() >= MAX_PLAYLISTS {
                    return Err(ms_common::Error::new("Too many playlists"));
                }
                d.playlists.push(pl);
                Ok(())
            })?;
            (user.name.clone(), made)
        };
        self.playlist_reply(user, &owner, &playlist)
    }

    pub(crate) fn update_playlist(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let id = p.req("playlistId")?.to_owned();
        let (owner, _) = self.find_playlist(user, &id)?;
        if owner != user.name && !user.admin {
            return Err(ApiError::not_authorized());
        }
        let add = self.track_ids(&p.all("songIdToAdd"))?;
        let mut remove: Vec<usize> = p
            .all("songIndexToRemove")
            .iter()
            .map(|s| {
                s.trim().parse::<usize>().map_err(|_| ApiError::bad_param("songIndexToRemove"))
            })
            .collect::<ApiResult<_>>()?;
        remove.sort_unstable();
        remove.dedup();
        let name = p.str("name").map(|n| truncate(n.trim(), MAX_NAME)).filter(|n| !n.is_empty());
        let comment = p.str("comment").map(|c| truncate(c, MAX_COMMENT));
        let public = p.bool("public");
        self.store.update(&owner, |d| {
            let pl =
                d.playlist_mut(&id).ok_or_else(|| ms_common::Error::new("Playlist not found"))?;
            for &i in remove.iter().rev() {
                if i < pl.entries.len() {
                    pl.entries.remove(i);
                }
            }
            if pl.entries.len() + add.len() > MAX_PLAYLIST_ENTRIES {
                return Err(ms_common::Error::new("Too many songs for one playlist"));
            }
            pl.entries.extend(add);
            if let Some(n) = name {
                pl.name = n;
            }
            if let Some(c) = comment {
                pl.comment = c;
            }
            if let Some(v) = public {
                pl.public = v;
            }
            pl.changed = now_ms();
            Ok(())
        })?;
        Ok(Reply::Ok)
    }

    pub(crate) fn delete_playlist(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let id = p.req("id")?.to_owned();
        let (owner, _) = self.find_playlist(user, &id)?;
        if owner != user.name && !user.admin {
            return Err(ApiError::not_authorized());
        }
        self.store.update(&owner, |d| {
            d.playlists.retain(|pl| pl.id != id);
            Ok(())
        })?;
        Ok(Reply::Ok)
    }
}
