# Design

music-server replaces Navidrome in the homelab. It serves one music library,
mounted read-only, to Subsonic apps for a handful of people, and it is built
under the same rules as playbook-runner:
- no third-party crates;
- hand-written FFI to libcrypto;
- every input bounded;
- every secret sealed or kept in memory.

## Crates

```text
ms-sys      libcrypto and POSIX declarations; the only crate with unsafe
ms-crypto   SHA-256, MD5 (Subsonic tokens only), HMAC, HKDF, AES-256-GCM
ms-common   bounded IO, JSON, base64, buffers, time, randomness
ms-tags     tag and stream-header parsers: ID3v1/v2, MPEG, FLAC, Vorbis
            comments, Ogg, MP4, WAV, JPEG/PNG dimensions
ms-library  the scan, the in-memory index, IDs, search, the index cache
ms-store    the sealed roster and per-user data
ms-http     HTTP/1.1: bounded requests, Range, file and streamed bodies
ms-media    ffmpeg supervision: transcodes and cover resizes
ms-subsonic the Subsonic API
ms-web      configuration, the status page, the binary
```

## Requests

`ms-http` runs a thread per connection, capped at 64, with one request per
connection.
- **Reading a request** has fixed ceilings: 32 KiB of headers, an 8 KiB
  request target, a 1 MiB body, and a 15 s read timeout.
- **Media responses** are sent without buffering. A file slice is copied by
  `std::io::copy` (sendfile). A transcode is pumped from ffmpeg's stdout
  through a 64 KiB buffer and framed by closing the connection.
- **Keep-alive is deliberately absent.** With a thread per connection, idle
  connections pooled by the proxy would pin threads against the ceiling. A
  new connection on the internal network costs almost nothing.

Every `/rest/<method>` call is handled the same way:
1. The call is authenticated.
2. It is dispatched by method name.
3. It is answered from one response tree, which renders either XML or JSON.

Media calls answer with bytes. Errors are Subsonic error documents with HTTP
200, as clients expect. An unknown method is logged by name only, never
with its parameters, so a missing feature an app relies on shows up in the
log and on the status page.

## The library

**Scan.** The scan walks the music directory:
- It never follows a symbolic link: entries are typed without following,
  and files are opened with `O_NOFOLLOW`.
- It skips hidden entries.
- It stops at a depth of 32 and at the track ceiling.
- A file whose size and modification time match the previous scan keeps its
  record without being opened.
- A changed file is probed by `ms-tags`. The probe reads the header region
  and seeks past embedded art, recording only where the art is.

**Index.** The records go through `build` into an immutable `Library`, held
behind an `Arc` and swapped atomically when a scan finishes. A request sees
one snapshot from start to end.

**Grouping.** Albums group by album artist and album name. Tracks without an
album-artist tag group by folder and album name, so an untagged compilation
stays one album. Its artist becomes the shared track artist, or "Various
Artists".

**Cache.** The scan's records are cached in `library.idx`, a checksummed
binary file that carries the format and parser versions. A restart serves
the library at once and the next scan opens only changed files. A version
mismatch or a damaged file discards the cache, and the next scan reads
everything.

**Memory.** An index entry costs roughly 300-500 bytes per track, so 50 000
tracks take about 25 MB, twice that briefly while a rescan builds its
replacement.

## Identifiers

Track, album, artist, and directory IDs are the kind prefix plus the first
96 bits of SHA-256 over a domain-separated description of the entity:
- a track by its path relative to the library root;
- an album by its grouping key;
- an artist by its folded name;
- a directory by its path.

They need no storage, survive rescans, rebuilt caches, reinstalls, and
master-key rotation, and match a phone's offline cache after any of those.
A moved or renamed file gets a new ID. Playlist entries that no longer
resolve are skipped when shown, not deleted, so they come back if the file
does. Playlist IDs are random.

## Accounts and authentication

The users file (a Podman secret written by Ansible) names every account with
its password and admin flag.
- **Passwords stay in memory**, cleansed on drop. Subsonic's token scheme,
  `t = md5(password || s)`, needs the plain password on the server. Storing
  one on disk, even sealed, would only add a copy to protect.
- **Comparisons are constant-time.** An unknown user is checked against a
  dummy password, so the reply takes the same time as for a real one.
- **Throttling** allows ten failures per address per fifteen minutes. The
  address is the right-most `X-Forwarded-For` entry when the connection
  comes from the trusted proxy, otherwise the peer.
- **The roster** (sealed in the data directory) remembers every account ever
  seen. Each start syncs it to the file. A removed account is disabled
  rather than deleted, so its data survives a mistake in the inventory.

**The status page** is for admins. It uses in-memory sessions keyed by
HMAC(web key, id), HMAC-derived CSRF tokens, `SameSite=Strict` cookies, an
origin check on every POST, and a five-failure login throttle. The page has
no JavaScript and is served under a CSP that refuses scripts, inline styles,
and third-party origins.

## Stored data

```text
<data>/users.sealed          the roster
<data>/user/<hash>.sealed    one user's playlists, stars, plays
<cache>/library.idx          the index cache (plain; holds nothing secret)
<cache>/covers/...           resized covers (plain)
```

**Sealing.** Sealed files are `"MSF1" || nonce || AES-256-GCM(ciphertext) ||
tag`. The key is HKDF-derived from the master key for this purpose. The
authenticated label is the file's role (`users`, `user/<name>`), so a file
copied over another fails to open.

**Writes.** Playlist and star changes are written atomically before the
request returns: write a temporary file, fsync it, rename it, fsync the
directory. Play counts change on every scrobble, so a flush thread writes
them every five seconds.

## ffmpeg

`ms-media` is the only crate that starts a process, and the gate enforces
it. Every invocation:
- **Fixed arguments.** A fixed argument vector, never a shell line.
- **No URLs or options from file names.** `-protocol_whitelist file` and a
  `file:` input, so a file name can be neither an option nor a URL.
- **A bare environment.** The environment is cleared, stdin is `/dev/null`,
  and the working directory is `/`.
- **Supervision.** It runs in its own process group. Dropping the
  supervising handle kills the group with SIGKILL and reaps it. A client
  that hangs up makes the next write fail, which drops the handle.
- **A deadline.** A watchdog kills it at a wall-clock deadline: twice the
  track's remaining length plus a minute for a transcode, 15 s for a resize.
- **A cap.** At most `MS_MAX_TRANSCODES` transcodes and
  `MS_MAX_COVER_RESIZES` resizes run at once.

**When slots are full.** A stream that would only have been transcoded to
meet a bitrate limit falls back to the original file. A stream whose app
insisted on a format gets a 503 to retry. A cover falls back to the
unresized original.

**Transcode output.** MP3 is written without an ID3 tag or Xing frame. On a
pipe the muxer cannot go back and fill one in. With `estimateContentLength`,
the output is cut or padded to exactly the promised length.

**Resized covers.** Resized covers are cached by source and size bucket, in
a capped least-recently-used directory.

## Things deliberately left out

These Navidrome features weren't in use and aren't here:
- a web player;
- sharing;
- external scrobbling;
- smart playlists;
- multiple libraries;
- user management in the UI;
- ratings.

The Subsonic calls for them answer with empty lists, so apps degrade
quietly.
