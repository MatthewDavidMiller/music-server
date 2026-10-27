# Usage

## Connecting an app

Any Subsonic-compatible app works; play:Sub (iOS) is the one this server is
tested against. In the app, add a server:

| Setting | Value |
|---|---|
| Server address | `https://music.<your domain>` |
| Username | your account name from the users file |
| Password | your password from the users file |

Apps log in with Subsonic's token scheme (`md5(password + salt)`) or by
sending the password itself. Both are accepted; either way the password
travels inside an HTTPS request. Because the Subsonic protocol needs the
server to know the plain password, **use a password for this server that
you use nowhere else.**

Ten failed logins from one address within fifteen minutes lock that address
out until the oldest failure is fifteen minutes old.

## What works

| Feature | Notes |
|---|---|
| Browsing by artist, album, genre, folder | Album artists group albums. Without an album-artist tag, an album is the tracks with one album name in one folder. |
| Search | Every word must appear in the title, artist, or album, in any case and with or without accents. |
| Album lists | Newest, random, alphabetical by name or artist, by year, by genre, most played, recently played, starred. |
| Streaming | The original file, with seeking. With a bitrate limit (as on cellular), files above the limit are transcoded to MP3, or Opus if the app asks for it. |
| Offline downloads | `download` always sends the original file. |
| Cover art | Embedded pictures and folder images (`cover`, `folder`, `front`, `album`, `albumart` with `.jpg`, `.jpeg`, or `.png`). Resized for the size the app asks for, and cached. |
| Playlists | Create, edit, reorder by removing and re-adding, delete. Public playlists are visible, read-only, to everyone. |
| Stars and play counts | Per user. Plays are counted from the app's scrobbles. Nothing is sent to Last.fm or anywhere else. |
| Now playing | Who is playing what. |

Not supported, and answered with an empty list rather than an error:
podcasts, internet radio, shares, bookmarks, the play queue, lyrics, ratings,
artist biographies, similar songs, jukebox, and chat.

## New music

Copy files into the library folder (in the homelab, the Nextcloud `Music`
folder). The server rescans every hour, reading only files that are new or
changed. An administrator can start a scan sooner from the app (if it offers
"scan library") or by restarting the service.

A file that cannot be read is listed with the reason on the status page.
Hidden files and folders (names starting with `.`) and symbolic links are
skipped.

## The status page

Open `https://music.<your domain>/` in a browser and log in with an
administrator account. The page shows the library's size, the last scan and
any problem files, the accounts (including ones disabled because they were
removed from the users file), current transcodes, and any Subsonic calls an
app made that this server does not implement. Nothing can be changed from
the page.
