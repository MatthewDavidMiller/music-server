# Deployment

How music-server runs in production. In the homelab, the `music_server` role
in `ansible_playbooks` does all of this on VM1; this page is the reference
for what that role sets up and why.

---

## The container

```bash
podman run -d --name music_server \
  --read-only \
  --cap-drop=ALL \
  --security-opt=no-new-privileges:true \
  --memory=512m --memory-swap=512m \
  --pids-limit=200 \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,size=64m \
  --user 10001:10001 --group-add 33 \
  --secret music_server_master_key,target=/run/secrets/music_server_master_key,uid=10001,gid=10001,mode=0400 \
  --secret music_server_users,target=/run/secrets/music_server_users,uid=10001,gid=10001,mode=0400 \
  -v /srv/nextcloud/data/admin/files/Music:/music:ro,z \
  -v /opt/music-server/data:/var/lib/music-server:Z \
  -v /opt/music-server/cache:/var/cache/music-server:Z \
  -e MS_TRUSTED_PROXIES=<traefik address> \
  ghcr.io/matthewdavidmiller/music-server@sha256:<digest>
```

| Flag | Why |
|---|---|
| `--read-only` | Nothing in the image is modified at runtime. |
| `--cap-drop=ALL`, `no-new-privileges` | The server runs as uid 10001 and needs no capabilities; it listens on 8080. |
| `--memory` = `--memory-swap` | No swap, so passwords in memory are never paged to disk. 512 MiB covers the index of a large library, two transcodes, and two cover resizes. |
| `--pids-limit` | Connection threads plus ffmpeg, which runs single-threaded. |
| tmpfs `/tmp` | Embedded cover art is staged here for ffmpeg to resize. `noexec` is fine: ffmpeg runs from the image. |
| `--group-add 33` | Nextcloud's files are `0660`/`0770` for uid and gid 33; the extra group lets uid 10001 read them without owning them. |
| `-v ...:/music:ro` | The library, read-only. |
| `-v .../data` | Sealed roster and user data. **Back this up.** |
| `-v .../cache` | The library index and resized covers. Rebuildable; don't back it up. |

The container needs no outbound network access and no published ports.
Traefik reaches it on an internal Podman network and terminates HTTPS. Turn
off Traefik's access log for this route: Subsonic puts credentials in the
query string, which an access log would record.

## Configuration

Environment variables, all optional. A value that is set but invalid stops
the server.

| Variable | Default | Meaning |
|---|---|---|
| `MS_LISTEN` | `0.0.0.0:8080` (image) | Listener address |
| `MS_MUSIC_DIR` | `/music` | The library |
| `MS_DATA_DIR` | `/var/lib/music-server` | Sealed state |
| `MS_CACHE_DIR` | `/var/cache/music-server` | Index and cover cache |
| `MS_MASTER_KEY_FILE` | `/run/secrets/music_server_master_key` | 64 hex digits |
| `MS_USERS_FILE` | `/run/secrets/music_server_users` | The accounts |
| `MS_TRUSTED_PROXIES` | unset | Comma-separated proxy IPs whose `X-Forwarded-For` is believed (for throttling) |
| `MS_SECURE_COOKIES` | `true` | Leave on behind HTTPS |
| `MS_SESSION_HOURS` | `12` | Status page session lifetime (1-720) |
| `MS_SCAN_INTERVAL_MINUTES` | `60` | Rescan interval; `0` for startup and on request only |
| `MS_MAX_TRACKS` | `200000` | The most tracks a scan collects |
| `MS_FFMPEG` | `/usr/bin/ffmpeg` | ffmpeg |
| `MS_MAX_TRANSCODES` | `2` | Concurrent transcodes (0-8; `0` streams originals only) |
| `MS_MAX_COVER_RESIZES` | `2` | Concurrent resizes (0-8; `0` serves originals) |
| `MS_TMP_DIR` | `/tmp` | Staging for cover resizes |
| `MS_DEFAULT_TRANSCODE_FORMAT` | `mp3` | `mp3` or `opus`, when an app sets only a bitrate limit |
| `MS_TRANSCODE_BITRATE` | `192` | kbit/s, when an app names a format without a limit |
| `MS_COVER_CACHE_MB` | `256` | Resized cover cache ceiling (`0` disables it) |
| `MS_ALLOW_PASSWORD_AUTH` | `true` | Accept `p=` logins as well as tokens |

## Accounts

The users file is JSON:

```json
{"version": 1, "users": [
  {"name": "alice", "password": "a long unique password", "admin": true},
  {"name": "bob", "password": "another one"}
]}
```

- Names are 1-64 characters from `A-Z a-z 0-9 . _ -`.
- Passwords are 8-128 bytes.
- At least one account must be an admin.
- At most 50 accounts.

The server refuses to start on an invalid file, and the error names the
entry, never a password. Check a file before deploying it:

```bash
podman run --rm -v ./users.json:/u.json:ro,Z -e MS_USERS_FILE=/u.json \
  --entrypoint music-server ghcr.io/matthewdavidmiller/music-server@sha256:<digest> check-users
```

The file is read at startup, so restart the service after changing it.
Changes take effect as follows:

| Change | Effect |
|---|---|
| New name | The account is added. |
| Removed name | The account is disabled, and its playlists, stars, and plays are kept. |
| Name added back | The account is enabled again, with its old data. |
| Admin flag | Takes effect at once. |
| Password | Takes effect at once. Only the file holds it. |

## The master key

User data and the roster are sealed with AES-256-GCM under keys derived from
one 256-bit master key. Generate it once:

```bash
podman run --rm --entrypoint music-server ghcr.io/matthewdavidmiller/music-server@sha256:<digest> gen-key
```

Keep it in the inventory's vault (it becomes the Podman secret) and in
Vaultwarden. Without it, the data directory is unreadable.

The server refuses to start when the key does not match the one the data
directory was sealed with. Library IDs do not depend on the key.

## Backups and restore

Back up the data directory: the roster and one file per user. A daily
tarball, as the Ansible role makes, is enough. To restore:

1. Stop the service.
2. Unpack the tarball into the data directory, owned by 10001.
3. Make sure the same master key is in place.
4. Start the service.

The cache rebuilds by itself; the first scan after losing it reads every
file.

## Checking a library before deploying

`scan-report` scans a directory once, without writing anything. It reports
the counts, the files it could not read and why, and the peak memory used:

```bash
podman run --rm --read-only -v /path/to/Music:/music:ro,z \
  --entrypoint music-server ghcr.io/matthewdavidmiller/music-server@sha256:<digest> scan-report
```

## Upgrading

Releases are images pinned by digest. `ansible_playbooks` updates the lock
entry with `scripts/review_container_updates.sh --service music_server`. The
data directory needs no migration between releases; if a parser change
would read files differently, the index cache is rebuilt automatically.

## Releasing

`make release` runs on the host. It needs:
- a clean tree on `main`;
- `podman login ghcr.io` with `write:packages`;
- an authenticated `gh`.

It runs the gate and the image tests, pushes
`ghcr.io/matthewdavidmiller/music-server:<version>`, tags `v<version>`, and
creates a GitHub release whose notes carry the digest. Bump the workspace
version in `Cargo.toml` first.
