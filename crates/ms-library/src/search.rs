//! Search: every query word must appear in the entity's folded text (title,
//! artist, and album for a track). An empty query matches everything,
//! which is how Subsonic clients page through a whole library to sync it.

use crate::fold::fold;
use crate::model::Library;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query(Vec<String>);

impl Query {
    /// Folds and splits a client's query. Quotes and the `*` some clients
    /// add for "anything" are ignored.
    pub fn parse(text: &str) -> Self {
        let cleaned: String =
            text.chars().map(|c| if c == '"' || c == '*' { ' ' } else { c }).collect();
        Self(fold(&cleaned).split(' ').filter(|w| !w.is_empty()).map(str::to_owned).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn matches(&self, haystack: &str) -> bool {
        self.0.iter().all(|w| haystack.contains(w.as_str()))
    }
}

/// Indices `offset..offset + count` of the matching items, in order.
fn page<'a>(
    hay: impl Iterator<Item = (u32, &'a str)>,
    q: &Query,
    offset: usize,
    count: usize,
) -> Vec<u32> {
    hay.filter(|(_, h)| q.matches(h)).skip(offset).take(count).map(|(i, _)| i).collect()
}

impl Library {
    pub fn search_tracks(&self, q: &Query, offset: usize, count: usize) -> Vec<u32> {
        page((0_u32..).zip(self.tracks.iter().map(|t| t.search.as_str())), q, offset, count)
    }

    pub fn search_albums(&self, q: &Query, offset: usize, count: usize) -> Vec<u32> {
        page(
            self.albums_by_name.iter().map(|&a| (a, self.album(a).search.as_str())),
            q,
            offset,
            count,
        )
    }

    pub fn search_artists(&self, q: &Query, offset: usize, count: usize) -> Vec<u32> {
        page(
            self.artists_sorted.iter().map(|&a| (a, self.artist(a).search.as_str())),
            q,
            offset,
            count,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{build, tests::rec};

    #[test]
    fn queries() {
        let lib = build(
            vec![
                rec("1.mp3", Some("Jóga"), Some("Björk"), Some("Homogenic"), None, 1),
                rec("2.mp3", Some("Hunter"), Some("Björk"), Some("Homogenic"), None, 2),
                rec("3.mp3", Some("Teardrop"), Some("Massive Attack"), Some("Mezzanine"), None, 1),
            ],
            vec![],
            0,
        );
        let titles = |q: &str| -> Vec<String> {
            lib.search_tracks(&Query::parse(q), 0, 10)
                .iter()
                .map(|&t| lib.track(t).title.clone())
                .collect()
        };
        assert_eq!(titles("bjork joga"), ["Jóga"]);
        assert_eq!(titles("HOMO"), ["Jóga", "Hunter"]);
        assert_eq!(titles("\"\""), ["Jóga", "Hunter", "Teardrop"]);
        assert_eq!(titles("nothing"), Vec::<String>::new());
        assert_eq!(lib.search_tracks(&Query::parse(""), 1, 1).len(), 1);
        assert_eq!(lib.search_albums(&Query::parse("mezz"), 0, 10).len(), 1);
        assert_eq!(lib.search_artists(&Query::parse("massive"), 0, 10).len(), 1);
        assert!(Query::parse(" * ").is_empty());
    }
}
