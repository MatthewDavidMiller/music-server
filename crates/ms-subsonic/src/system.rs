//! Calls about the server itself, and empty answers for the features this
//! server does not have (podcasts, radio, shares, bookmarks, lyrics, ...)
//! so that a client's screens for them show "nothing" rather than an error.

use crate::encode::{Node, Val};
use crate::error::{ApiError, ApiResult};
use crate::model::iso_ms;
use crate::{Api, Reply, User};

pub(crate) fn license() -> Node {
    Node::new("license")
        .b("valid", true)
        .s("email", "")
        .s("licenseExpires", "2999-12-31T00:00:00.000Z")
}

pub(crate) fn extensions() -> Node {
    let ext = |name: &str| {
        Node::new("openSubsonicExtensions").s("name", name).values("versions", vec![Val::Int(1)])
    };
    // A bare list in the envelope, not an element holding one.
    Node::new("").list("openSubsonicExtensions", vec![ext("formPost"), ext("transcodeOffset")])
}

pub(crate) fn music_folders() -> Node {
    Node::new("musicFolders")
        .list("musicFolder", vec![Node::new("musicFolder").i("id", 1).s("name", "Music")])
}

/// Valid, empty responses for calls about features this server lacks.
pub(crate) fn stub(method: &str) -> Option<Reply> {
    let empty = |name: &'static str| Some(Reply::Node(Node::new(name)));
    match method {
        "getPodcasts" => empty("podcasts"),
        "getNewestPodcasts" => empty("newestPodcasts"),
        "getInternetRadioStations" => empty("internetRadioStations"),
        "getBookmarks" => empty("bookmarks"),
        "getShares" => empty("shares"),
        "getArtistInfo" => empty("artistInfo"),
        "getArtistInfo2" => empty("artistInfo2"),
        "getAlbumInfo" | "getAlbumInfo2" => empty("albumInfo"),
        "getSimilarSongs" => empty("similarSongs"),
        "getSimilarSongs2" => empty("similarSongs2"),
        "getTopSongs" => empty("topSongs"),
        "getLyrics" => empty("lyrics"),
        "getVideos" => empty("videos"),
        "getChatMessages" => empty("chatMessages"),
        "getPlayQueue" | "createBookmark" | "deleteBookmark" | "addChatMessage" => Some(Reply::Ok),
        _ => None,
    }
}

impl Api {
    pub(crate) fn scan_status(&self) -> Node {
        self.scan_status_node(self.library.status().scanning)
    }

    fn scan_status_node(&self, scanning: bool) -> Node {
        let status = self.library.status();
        let lib = self.library.library();
        let mut n = Node::new("scanStatus")
            .b("scanning", scanning)
            .u("count", if status.scanning { status.files_seen } else { lib.tracks.len() as u64 })
            .i("folderCount", 1);
        if lib.scanned_at_ms > 0 {
            n = n.s("lastScan", iso_ms(i64::try_from(lib.scanned_at_ms).unwrap_or(0)));
        }
        n
    }

    pub(crate) fn start_scan(&self, user: &User) -> ApiResult<Reply> {
        if !user.admin {
            return Err(ApiError::not_authorized());
        }
        self.library.request_scan();
        Ok(Reply::Node(self.scan_status_node(true)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{Format, render};

    #[test]
    fn extensions_render_as_opensubsonic_expects() {
        let json = render(Format::Json, Some(extensions()), true);
        assert!(
            json.contains(",\"openSubsonicExtensions\":[{\"name\":\"formPost\",\"versions\":[1]}"),
            "{json}"
        );
        let xml = render(Format::Xml, Some(extensions()), true);
        assert!(xml.contains("><openSubsonicExtensions name=\"formPost\"><versions>1</versions></openSubsonicExtensions>"), "{xml}");
        assert!(stub("getPodcasts").is_some() && stub("getNothing").is_none());
    }
}
