# music-server

A small music streaming server for a home library. It speaks the Subsonic
API, so phone apps such as play:Sub, Symfonium, and Amperfy can browse the
library, stream it, download it for offline use, and keep playlists.

- **One library, several accounts.** The accounts come from a users file
  (an Ansible-managed Podman secret). Each person has their own playlists,
  stars, and play counts.
- **Reads your files as they are.** MP3, FLAC, AAC and ALAC (M4A), Ogg
  Vorbis, Opus, and WAV. Tags, durations, and embedded or folder cover art
  are read by hand-written parsers. The music directory is mounted
  read-only and never modified.
- **Transcodes on demand.** When a phone asks for a lower bitrate, ffmpeg
  turns the file into MP3 or Opus as it streams. Otherwise the original
  file is sent, with seeking.
- **No database.** The library is an in-memory index rebuilt from a scan
  cache. Per-user data is a few small files, sealed with AES-256-GCM under
  one master key.
- **A status page** for administrators shows scans, problem files,
  accounts, and activity.

It is first-party Rust with no third-party crates. OpenSSL's libcrypto, bound
by hand in `crates/ms-sys`, is the only third-party library linked. ffmpeg,
from the official Alpine Linux image the container is built on, is the only
third-party program it runs.

## Documentation

| Page | For |
|---|---|
| [docs/USAGE.md](docs/USAGE.md) | Connecting a Subsonic app; what is supported |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Container flags, configuration, accounts, the master key, backups |
| [docs/DESIGN.md](docs/DESIGN.md) | Architecture and the security model |
| [AGENTS.md](AGENTS.md) | Rules for working on the code |

## Try it

Rootless Podman is the only prerequisite. Every compiler, linter, and the
ffmpeg the tests use run in a toolbox image.

```bash
make install   # once: build the toolbox and install the git hook
mkdir music-dev && cp -r ~/Music/SomeAlbum music-dev/
make dev       # serve http://127.0.0.1:8080, state in ./data
make test      # the full gate
```

`make dev` creates the account `admin` with the password `dev-password`.
Point a Subsonic app at `http://127.0.0.1:8080`, or open that address in a
browser for the status page. `make help` lists the other targets (`image`,
`test-image`, `release`).

## License

Copyright (c) 2026 Matthew David Miller.
[Licensed under the MIT License.](LICENSE)
