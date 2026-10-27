//! Browsing: the folder view (`getIndexes`, `getMusicDirectory`), the ID3
//! view (`getArtists`, `getArtist`, `getAlbum`, `getSong`), genres, and
//! search.

use std::collections::BTreeMap;

use ms_library::fold::index_letter;
use ms_library::{Entity, Kind, Query};

use crate::encode::Node;
use crate::error::{ApiError, ApiResult};
use crate::model::View;
use crate::params::{MAX_PAGE, Params};
use crate::{Api, Reply, User};

const IGNORED_ARTICLES: &str = "The El La Los Las Le Les";

/// Groups `(name, node)` pairs into `<index name="A">` elements.
fn indexed(items: Vec<(String, Node)>) -> Vec<Node> {
    let mut groups: BTreeMap<char, Vec<Node>> = BTreeMap::new();
    for (name, node) in items {
        groups.entry(index_letter(&name)).or_default().push(node);
    }
    // "#" sorts after the letters, as clients show it.
    let (hash, letters): (Vec<_>, Vec<_>) = groups.into_iter().partition(|(c, _)| *c == '#');
    letters
        .into_iter()
        .chain(hash)
        .map(|(c, nodes)| Node::new("index").s("name", c.to_string()).list("artist", nodes))
        .collect()
}

impl Api {
    pub(crate) fn get_indexes(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        self.with_view(user, |v| {
            let lib = v.lib;
            let modified = i64::try_from(lib.scanned_at_ms).unwrap_or(0);
            let mut n = Node::new("indexes")
                .i("lastModified", modified)
                .s("ignoredArticles", IGNORED_ARTICLES);
            if p.int("ifModifiedSince")?.is_some_and(|since| since >= modified && modified > 0) {
                return Ok(Reply::Node(n));
            }
            let Some(root) = lib.root() else { return Ok(Reply::Node(n)) };
            let items = root
                .dirs
                .iter()
                .map(|&d| {
                    let dir = lib.dir(d);
                    let mut node =
                        Node::new("artist").s("id", dir.id.to_string()).s("name", dir.name.clone());
                    if let Some(s) = v.ud.starred_at(&dir.id.to_string()) {
                        node = node.s("starred", crate::model::iso_ms(s));
                    }
                    (dir.name.clone(), node)
                })
                .collect();
            n = n.list("index", indexed(items));
            n = n.list("child", root.tracks.iter().map(|&t| v.song("child", t)).collect());
            Ok(Reply::Node(n))
        })
    }

    pub(crate) fn get_music_directory(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let id = p.id("id")?;
        self.with_view(user, |v| {
            let lib = v.lib;
            let dir = match lib.lookup(&id) {
                Some(Entity::Dir(d)) => {
                    let dir = lib.dir(d);
                    let mut n = Node::new("directory").s("id", dir.id.to_string());
                    if let Some(parent) = dir.parent {
                        n = n.s("parent", lib.dir(parent).id.to_string());
                    }
                    n.s("name", dir.name.clone()).list(
                        "child",
                        dir.dirs
                            .iter()
                            .map(|&c| v.dir_child(c))
                            .chain(dir.tracks.iter().map(|&t| v.song("child", t)))
                            .collect(),
                    )
                }
                Some(Entity::Artist(a)) => {
                    let artist = lib.artist(a);
                    Node::new("directory")
                        .s("id", artist.id.to_string())
                        .s("name", artist.name.clone())
                        .list(
                            "child",
                            artist.albums.iter().map(|&al| v.album_dir("child", al)).collect(),
                        )
                }
                Some(Entity::Album(a)) => {
                    let album = lib.album(a);
                    Node::new("directory")
                        .s("id", album.id.to_string())
                        .s("parent", lib.artist(album.artist_ix).id.to_string())
                        .s("name", album.name.clone())
                        .list("child", album.tracks.iter().map(|&t| v.song("child", t)).collect())
                }
                _ => return Err(ApiError::not_found("Directory")),
            };
            Ok(Reply::Node(dir))
        })
    }

    pub(crate) fn get_artists(&self, user: &User) -> ApiResult<Reply> {
        self.with_view(user, |v| {
            let items = v
                .lib
                .artists_sorted
                .iter()
                .map(|&a| (v.lib.artist(a).name.clone(), v.artist("artist", a)))
                .collect();
            Ok(Reply::Node(
                Node::new("artists")
                    .s("ignoredArticles", IGNORED_ARTICLES)
                    .list("index", indexed(items)),
            ))
        })
    }

    pub(crate) fn get_artist(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let id = p.id("id")?;
        self.with_view(user, |v| {
            let Some(Entity::Artist(a)) = v.lib.lookup(&id) else {
                return Err(ApiError::not_found("Artist"));
            };
            let albums = v.lib.artist(a).albums.iter().map(|&al| v.album("album", al)).collect();
            Ok(Reply::Node(v.artist("artist", a).list("album", albums)))
        })
    }

    pub(crate) fn get_album(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let id = p.id("id")?;
        self.with_view(user, |v| {
            let Some(Entity::Album(a)) = v.lib.lookup(&id) else {
                return Err(ApiError::not_found("Album"));
            };
            let songs = v.lib.album(a).tracks.iter().map(|&t| v.song("song", t)).collect();
            Ok(Reply::Node(v.album("album", a).list("song", songs)))
        })
    }

    pub(crate) fn get_song(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let id = p.id("id")?;
        self.with_view(user, |v| match v.lib.lookup(&id) {
            Some(Entity::Track(t)) => Ok(Reply::Node(v.song("song", t))),
            _ => Err(ApiError::not_found("Song")),
        })
    }

    pub(crate) fn get_genres(&self) -> Node {
        let lib = self.library.library();
        let genres = lib
            .genres
            .iter()
            .map(|g| {
                Node::new("genre")
                    .i("songCount", g.songs)
                    .i("albumCount", g.albums)
                    .text(g.name.clone())
            })
            .collect();
        Node::new("genres").list("genre", genres)
    }

    pub(crate) fn search(&self, p: &Params<'_>, user: &User, id3: bool) -> ApiResult<Reply> {
        let q = Query::parse(p.str("query").unwrap_or(""));
        let artist_count = p.count("artistCount", 20, MAX_PAGE)?;
        let album_count = p.count("albumCount", 20, MAX_PAGE)?;
        let song_count = p.count("songCount", 20, MAX_PAGE)?;
        let (ao, bo, so) =
            (p.offset("artistOffset")?, p.offset("albumOffset")?, p.offset("songOffset")?);
        self.with_view(user, |v: &View<'_>| {
            let lib = v.lib;
            let artists = lib.search_artists(&q, ao, artist_count);
            let albums = lib.search_albums(&q, bo, album_count);
            let songs = lib.search_tracks(&q, so, song_count);
            let name = if id3 { "searchResult3" } else { "searchResult2" };
            let album_node =
                |a: u32| if id3 { v.album("album", a) } else { v.album_dir("album", a) };
            Ok(Reply::Node(
                Node::new(name)
                    .list("artist", artists.iter().map(|&a| v.artist("artist", a)).collect())
                    .list("album", albums.iter().map(|&a| album_node(a)).collect())
                    .list("song", songs.iter().map(|&t| v.song("song", t)).collect()),
            ))
        })
    }
}

/// Whether `id` is of a kind that can be starred.
pub(crate) fn starrable(kind: Kind) -> bool {
    matches!(kind, Kind::Track | Kind::Album | Kind::Artist | Kind::Dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_groups_put_hash_last() {
        let idx = indexed(vec![
            ("2Pac".into(), Node::new("artist").s("name", "2Pac")),
            ("The Beatles".into(), Node::new("artist").s("name", "The Beatles")),
            ("ABBA".into(), Node::new("artist").s("name", "ABBA")),
        ]);
        let names: Vec<String> = idx
            .iter()
            .map(|n| match &n.attrs[0].1 {
                crate::encode::Val::Str(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(names, ["A", "B", "#"]);
    }
}
