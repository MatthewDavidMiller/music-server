//! Per-user calls: scrobbles (local play counts only; nothing is sent to
//! any outside service), stars, now playing, and account details.

use ms_common::timefmt;
use ms_library::{Entity, Id};
use ms_store::lock;

use crate::browse::starrable;
use crate::encode::{Node, Val};
use crate::error::{ApiError, ApiResult};
use crate::params::Params;
use crate::{Api, Reply, User};

impl Api {
    pub(crate) fn scrobble(&self, p: &Params<'_>, user: &User, client: &str) -> ApiResult<Reply> {
        let ids = p.ids("id")?;
        if ids.is_empty() {
            return Err(ApiError::missing("id"));
        }
        let times = p.all("time");
        let submission = p.bool("submission").unwrap_or(true);
        let lib = self.library.library();
        let now = timefmt::unix_ms();
        let mut plays = Vec::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            if lib.track_by_id(id).is_none() {
                return Err(ApiError::not_found("Song"));
            }
            let at = times
                .get(i)
                .and_then(|t| t.parse::<i64>().ok())
                .filter(|&t| t > 0 && t <= now + 86_400_000)
                .unwrap_or(now);
            plays.push((id.to_string(), at));
        }
        if submission {
            self.store.update_later(&user.name, |d| {
                for (id, at) in &plays {
                    d.record_play(id, *at);
                }
            })?;
        } else if let Some(last) = ids.last() {
            self.set_now_playing(&user.name, *last, client);
        }
        Ok(Reply::Ok)
    }

    pub(crate) fn star(&self, p: &Params<'_>, user: &User, on: bool) -> ApiResult<Reply> {
        let mut ids: Vec<Id> = p.ids("id")?;
        ids.extend(p.ids("albumId")?);
        ids.extend(p.ids("artistId")?);
        if ids.is_empty() {
            return Err(ApiError::missing("id"));
        }
        let lib = self.library.library();
        for id in &ids {
            if !starrable(id.kind) || lib.lookup(id).is_none() {
                return Err(ApiError::not_found("Item"));
            }
        }
        let at = timefmt::unix_ms();
        self.store.update(&user.name, |d| {
            for id in &ids {
                let key = id.to_string();
                if on {
                    d.star(&key, at)?;
                } else {
                    d.unstar(&key);
                }
            }
            Ok(())
        })?;
        Ok(Reply::Ok)
    }

    fn user_node(name: &str, admin: bool) -> Node {
        Node::new("user")
            .s("username", name)
            .s("email", "")
            .b("scrobblingEnabled", true)
            .b("adminRole", admin)
            .b("settingsRole", false)
            .b("downloadRole", true)
            .b("uploadRole", false)
            .b("playlistRole", true)
            .b("coverArtRole", true)
            .b("commentRole", false)
            .b("podcastRole", false)
            .b("streamRole", true)
            .b("jukeboxRole", false)
            .b("shareRole", false)
            .b("videoConversionRole", false)
            .values("folder", vec![Val::Int(1)])
    }

    pub(crate) fn get_user(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let name = p.str("username").filter(|u| !u.is_empty()).unwrap_or(&user.name);
        if name != user.name && !user.admin {
            return Err(ApiError::not_authorized());
        }
        if !self.roster.iter().any(|r| r.name == name && r.enabled) {
            return Err(ApiError::not_found("User"));
        }
        Ok(Reply::Node(Self::user_node(name, self.credentials.is_admin(name))))
    }

    pub(crate) fn get_users(&self, user: &User) -> ApiResult<Reply> {
        if !user.admin {
            return Err(ApiError::not_authorized());
        }
        let users = self
            .roster
            .iter()
            .filter(|r| r.enabled)
            .map(|r| Self::user_node(&r.name, self.credentials.is_admin(&r.name)))
            .collect();
        Ok(Reply::Node(Node::new("users").list("user", users)))
    }

    pub(crate) fn get_now_playing(&self, user: &User) -> ApiResult<Reply> {
        let now = timefmt::now_ms();
        // An entry older than an hour is stale.
        let list: Vec<_> = lock(&self.now_playing)
            .iter()
            .filter(|n| now.saturating_sub(n.at_ms) < 3_600_000)
            .cloned()
            .collect();
        self.with_view(user, |v| {
            let entries = list
                .iter()
                .filter_map(|n| match v.lib.lookup(&n.track) {
                    Some(Entity::Track(t)) => Some(
                        v.song("entry", t)
                            .s("username", n.user.clone())
                            .u("minutesAgo", now.saturating_sub(n.at_ms) / 60_000)
                            .i("playerId", 0)
                            .s("playerName", n.client.clone()),
                    ),
                    _ => None,
                })
                .collect();
            Ok(Reply::Node(Node::new("nowPlaying").list("entry", entries)))
        })
    }
}
