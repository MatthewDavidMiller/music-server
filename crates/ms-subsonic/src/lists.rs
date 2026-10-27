//! Album and song lists: `getAlbumList(2)`, `getRandomSongs`,
//! `getSongsByGenre`, and `getStarred(2)`.

use std::cmp::Reverse;
use std::collections::HashMap;

use ms_library::fold::fold;
use ms_library::{Entity, Id, Library};
use ms_store::UserData;

use crate::encode::Node;
use crate::error::{ApiError, ApiResult};
use crate::params::{MAX_PAGE, Params};
use crate::{Api, Reply, User};

/// A small xorshift generator for shuffles, seeded from the OS.
pub(crate) struct Shuffle(u64);

impl Shuffle {
    pub(crate) fn new() -> Self {
        let seed =
            ms_common::rand::random_array::<8>().map_or(0x9e37_79b9_7f4a_7c15, u64::from_le_bytes);
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// `n` distinct items of `items`, in random order (a partial
    /// Fisher-Yates shuffle).
    pub(crate) fn pick<T: Copy>(&mut self, mut items: Vec<T>, n: usize) -> Vec<T> {
        let take = n.min(items.len());
        for i in 0..take {
            let span = (items.len() - i) as u64;
            let j = i + usize::try_from(self.next() % span).unwrap_or(0);
            items.swap(i, j);
        }
        items.truncate(take);
        items
    }
}

/// Per-album play totals for one user: (count, latest play).
fn album_plays(lib: &Library, ud: &UserData) -> HashMap<u32, (u64, i64)> {
    let mut out: HashMap<u32, (u64, i64)> = HashMap::new();
    for (id, play) in &ud.plays {
        if let Ok(id) = Id::parse(id)
            && let Some(t) = lib.track_by_id(&id)
        {
            let e = out.entry(t.album).or_default();
            e.0 += play.count;
            e.1 = e.1.max(play.last);
        }
    }
    out
}

fn album_genre_matches(lib: &Library, album: u32, genre: &str) -> bool {
    let wanted = fold(genre);
    lib.album(album)
        .tracks
        .iter()
        .any(|&t| lib.track(t).rec.genres.iter().any(|g| fold(g) == wanted))
}

impl Api {
    pub(crate) fn get_album_list(
        &self,
        p: &Params<'_>,
        user: &User,
        id3: bool,
    ) -> ApiResult<Reply> {
        let kind = p.req("type")?;
        let size = p.count("size", 10, MAX_PAGE)?;
        let offset = p.offset("offset")?;
        let from_year = p.int("fromYear")?;
        let to_year = p.int("toYear")?;
        let genre = p.str("genre");
        self.with_view(user, |v| {
            let lib = v.lib;
            let mut albums: Vec<u32> = match kind {
                "random" => Shuffle::new().pick(
                    (0..u32::try_from(lib.albums.len()).unwrap_or(0)).collect(),
                    offset + size,
                ),
                "newest" => lib.albums_by_created.clone(),
                "alphabeticalByName" => lib.albums_by_name.clone(),
                "alphabeticalByArtist" => lib.albums_by_artist.clone(),
                "highest" => Vec::new(),
                "frequent" | "recent" => {
                    let plays = album_plays(lib, v.ud);
                    let mut played: Vec<(u32, (u64, i64))> = plays.into_iter().collect();
                    if kind == "frequent" {
                        played.sort_by_key(|&(_, (count, last))| (Reverse(count), Reverse(last)));
                    } else {
                        played.sort_by_key(|&(_, (_, last))| Reverse(last));
                    }
                    played.into_iter().map(|(a, _)| a).collect()
                }
                "starred" => lib
                    .albums_by_name
                    .iter()
                    .copied()
                    .filter(|&a| v.ud.starred_at(&lib.album(a).id.to_string()).is_some())
                    .collect(),
                "byYear" => {
                    let (Some(from), Some(to)) = (from_year, to_year) else {
                        return Err(ApiError::missing("fromYear/toYear"));
                    };
                    let (lo, hi) = (from.min(to), from.max(to));
                    let mut list: Vec<u32> = lib
                        .albums_by_name
                        .iter()
                        .copied()
                        .filter(|&a| (lo..=hi).contains(&i64::from(lib.album(a).year)))
                        .collect();
                    list.sort_by_key(|&a| lib.album(a).year);
                    if from > to {
                        list.reverse();
                    }
                    list
                }
                "byGenre" => {
                    let genre = genre.ok_or_else(|| ApiError::missing("genre"))?;
                    lib.albums_by_name
                        .iter()
                        .copied()
                        .filter(|&a| album_genre_matches(lib, a, genre))
                        .collect()
                }
                _ => return Err(ApiError::bad_param("type")),
            };
            albums = albums.into_iter().skip(offset).take(size).collect();
            let name = if id3 { "albumList2" } else { "albumList" };
            let element = |a: u32| if id3 { v.album("album", a) } else { v.album_dir("album", a) };
            Ok(Reply::Node(
                Node::new(name).list("album", albums.iter().map(|&a| element(a)).collect()),
            ))
        })
    }

    pub(crate) fn get_random_songs(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let size = p.count("size", 10, MAX_PAGE)?;
        let genre = p.str("genre").map(fold);
        let from = p.int("fromYear")?;
        let to = p.int("toYear")?;
        self.with_view(user, |v| {
            let lib = v.lib;
            let candidates: Vec<u32> = (0..u32::try_from(lib.tracks.len()).unwrap_or(0))
                .filter(|&t| {
                    let r = &lib.track(t).rec;
                    genre.as_ref().is_none_or(|g| r.genres.iter().any(|x| fold(x) == *g))
                        && from.is_none_or(|y| i64::from(r.year) >= y)
                        && to.is_none_or(|y| i64::from(r.year) <= y)
                })
                .collect();
            let picked = Shuffle::new().pick(candidates, size);
            Ok(Reply::Node(
                Node::new("randomSongs")
                    .list("song", picked.iter().map(|&t| v.song("song", t)).collect()),
            ))
        })
    }

    pub(crate) fn get_songs_by_genre(&self, p: &Params<'_>, user: &User) -> ApiResult<Reply> {
        let genre = fold(p.req("genre")?);
        let count = p.count("count", 10, MAX_PAGE)?;
        let offset = p.offset("offset")?;
        self.with_view(user, |v| {
            let lib = v.lib;
            let songs: Vec<u32> = (0..u32::try_from(lib.tracks.len()).unwrap_or(0))
                .filter(|&t| lib.track(t).rec.genres.iter().any(|g| fold(g) == genre))
                .skip(offset)
                .take(count)
                .collect();
            Ok(Reply::Node(
                Node::new("songsByGenre")
                    .list("song", songs.iter().map(|&t| v.song("song", t)).collect()),
            ))
        })
    }

    pub(crate) fn get_starred(&self, user: &User, id3: bool) -> ApiResult<Reply> {
        self.with_view(user, |v| {
            let lib = v.lib;
            let mut stars = v.ud.starred.clone();
            stars.sort_by_key(|s| Reverse(s.at));
            let (mut artists, mut albums, mut songs) = (Vec::new(), Vec::new(), Vec::new());
            for s in &stars {
                let Ok(id) = Id::parse(&s.id) else { continue };
                match lib.lookup(&id) {
                    Some(Entity::Artist(a)) => artists.push(v.artist("artist", a)),
                    Some(Entity::Album(a)) => {
                        albums.push(if id3 {
                            v.album("album", a)
                        } else {
                            v.album_dir("album", a)
                        });
                    }
                    Some(Entity::Track(t)) => songs.push(v.song("song", t)),
                    Some(Entity::Dir(d)) if !id3 => {
                        let dir = lib.dir(d);
                        let mut n = Node::new("artist")
                            .s("id", dir.id.to_string())
                            .s("name", dir.name.clone());
                        n = n.s("starred", crate::model::iso_ms(s.at));
                        artists.push(n);
                    }
                    _ => {}
                }
            }
            let name = if id3 { "starred2" } else { "starred" };
            Ok(Reply::Node(
                Node::new(name).list("artist", artists).list("album", albums).list("song", songs),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffles_pick_distinct_items() {
        let mut s = Shuffle::new();
        let mut picked = s.pick((0..100).collect::<Vec<u32>>(), 10);
        assert_eq!(picked.len(), 10);
        picked.sort_unstable();
        picked.dedup();
        assert_eq!(picked.len(), 10);
        assert_eq!(s.pick(vec![1, 2], 10).len(), 2);
        assert!(s.pick(Vec::<u32>::new(), 3).is_empty());
    }
}
