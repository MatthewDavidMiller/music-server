# Agent Instructions

Workflow instructions for coding agents working in this repository. The
architecture and security model live in [docs/DESIGN.md](docs/DESIGN.md);
read it before changing anything. [docs/USAGE.md](docs/USAGE.md) and
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) describe the behaviour users and
operators rely on; update them with any change to it.

## Non-Negotiables

- **No third-party crates.** `Cargo.lock` lists workspace members only, and
  `scripts/check-rust.sh` fails the build otherwise. Write what you need, or
  port it from the sibling `playbook-runner` and `rpi-omt-client`
  repositories, which follow the same rule.
- **libcrypto is the only third-party library.** Never implement a
  cryptographic primitive; bind OpenSSL's in `crates/ms-sys` and wrap it in
  `crates/ms-crypto`. MD5 exists only because Subsonic's token protocol
  is defined with it.
- **ffmpeg is the only third-party program.** It is installed from Alpine in
  the runtime image and the toolbox, and is never linked. Only
  `crates/ms-media` starts it, with a fixed argument vector (no shell),
  `-protocol_whitelist file` and `file:` inputs, a cleared environment, its
  own process group killed on drop, a wall-clock deadline, and a
  concurrency cap. Nothing else may start a process, and the gate checks
  where `Command::new` appears.
- **`unsafe` lives only in `ms-sys`.** Every other crate root carries
  `#![forbid(unsafe_code)]`, and the gate checks it.
- **The music directory is read-only input.** Never write to it, never
  follow a symbolic link in it, and bound every read of it. Tag parsers
  treat every file as hostile.
- **Secrets never reach disk in plaintext, logs, or the UI.** Passwords
  live in memory only, loaded from the users file. User data is sealed.
  Never log a query string: Subsonic credentials travel in it.
- **Every input is bounded.** `clippy.toml` refuses the unbounded std
  readers; use the bounded replacements in `ms-common`.
- **Images are pinned by digest.** Base images in `Containerfile` and
  `tools/toolbox/Dockerfile` are official images referenced by digest.
- **Identifiers are stable.** Track, album, artist, and directory IDs are
  derived from the library itself (`crates/ms-library/src/id.rs`). Changing
  the derivation orphans every playlist entry and every client's cache.
  Don't change it.

## Validation

Every gate runs inside the toolbox image; rootless Podman is the only host
prerequisite. Never install a toolchain, linter, or ffmpeg on the host to
satisfy a gate.

```bash
make install   # once per clone
make test      # the full pre-commit gate
make test-image  # before a release: the image under VM1's flags
```

Run `make test` before reporting a change complete. Never pass `--no-verify`
to `git commit`.

## Commits

Only commit when asked. Commit messages are a short imperative subject
followed by a body explaining what was wrong, what changed, and why, wrapped
at 72 columns, no emoji.
