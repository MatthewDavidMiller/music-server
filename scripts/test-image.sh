#!/bin/bash
# Build the runtime image and test it the way VM1 runs it.
#
# Usage: make test-image   (runs on the host; needs rootless Podman)
#
# 1. Builds the image from the Containerfile.
# 2. Generates a small library with the image's own ffmpeg.
# 3. Starts the server with VM1's flags -- read-only root, every capability
#    dropped, no-new-privileges, a memory cap with no swap, a pids limit, a
#    noexec tmpfs /tmp, the music mounted read-only, and the master key and
#    users file as Podman secrets -- and checks it becomes healthy, runs as
#    uid 10001, cannot write its root filesystem, has the encoders it needs,
#    and answers Subsonic calls, including a transcoded stream.
# 4. Builds the end-to-end test binary statically and runs it inside the
#    same image under the same flags, against the image's own ffmpeg.

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${PROJECT_ROOT}"
IMAGE="localhost/music-server:test"
NAME="ms-image-test-$$"
KEY_SECRET="ms-image-test-key-$$"
USERS_SECRET="ms-image-test-users-$$"
DATA="ms-image-test-data-$$"
CACHE="ms-image-test-cache-$$"
PORT="${MS_TEST_PORT:-18081}"
WORK="$(mktemp -d)"

cleanup() {
    podman rm -f "${NAME}" >/dev/null 2>&1 || true
    podman secret rm "${KEY_SECRET}" "${USERS_SECRET}" >/dev/null 2>&1 || true
    podman volume rm -f "${DATA}" "${CACHE}" >/dev/null 2>&1 || true
    podman unshare rm -rf "${WORK}" 2>/dev/null || rm -rf "${WORK}"
}
trap cleanup EXIT

# The flags roles/music_server uses on VM1. --group-add stands in for the
# Nextcloud group the music is readable by.
# shellcheck disable=SC2054 # the commas are inside mount option lists
SECURITY=(
    --read-only
    --cap-drop=ALL
    --security-opt=no-new-privileges:true
    --memory=512m
    --memory-swap=512m
    --pids-limit=200
    --tmpfs /tmp:rw,noexec,nosuid,nodev,size=64m,mode=1777
)

echo "== build image"
podman build --format docker --platform linux/amd64 -q -f Containerfile -t "${IMAGE}" . >/dev/null

echo "== static binary"
podman run --rm --entrypoint sh "${IMAGE}" -c \
    'test "$(head -c 4 /usr/local/bin/music-server | od -An -c | tr -d " ")" = "177ELF"'

echo "== encoders"
encoders="$(podman run --rm --entrypoint ffmpeg "${IMAGE}" -hide_banner -encoders 2>/dev/null)"
for e in libmp3lame libopus mjpeg; do
    grep -qE "^ [A-Z.]{6} ${e} " <<<"${encoders}" || { echo "FAIL: ffmpeg lacks ${e}" >&2; exit 1; }
done

echo "== library"
mkdir -p "${WORK}/music/Artist/Album"
chmod -R a+rwX "${WORK}"
podman run --rm --userns=keep-id --user "$(id -u):$(id -g)" -v "${WORK}/music:/out:Z" \
    --entrypoint ffmpeg "${IMAGE}" \
    -hide_banner -loglevel error -f lavfi -i sine=frequency=440:duration=5 \
    -metadata title=Tone -metadata artist=Artist -metadata album=Album \
    -c:a flac /out/Artist/Album/01.flac
chmod -R a+rX "${WORK}/music"

echo "== smoke: start with VM1 flags"
(umask 077 && podman run --rm --entrypoint /usr/local/bin/music-server "${IMAGE}" gen-key > "${WORK}/key")
printf '%s' '{"version":1,"users":[{"name":"admin","password":"image-test-pw","admin":true}]}' > "${WORK}/users.json"
podman secret create "${KEY_SECRET}" "${WORK}/key" >/dev/null
podman secret create "${USERS_SECRET}" "${WORK}/users.json" >/dev/null
podman run -d --name "${NAME}" "${SECURITY[@]}" \
    --secret "${KEY_SECRET},target=/run/secrets/music_server_master_key,uid=10001,gid=10001,mode=0400" \
    --secret "${USERS_SECRET},target=/run/secrets/music_server_users,uid=10001,gid=10001,mode=0400" \
    -v "${WORK}/music:/music:ro,z" \
    -v "${DATA}:/var/lib/music-server" \
    -v "${CACHE}:/var/cache/music-server" \
    -p "127.0.0.1:${PORT}:8080" \
    "${IMAGE}" >/dev/null
for _ in $(seq 1 30); do
    podman exec "${NAME}" music-server healthcheck 2>/dev/null && break
    sleep 1
done
podman exec "${NAME}" music-server healthcheck
[[ "$(podman exec "${NAME}" id -u)" == 10001 ]] || { echo "FAIL: not uid 10001" >&2; exit 1; }
if podman exec "${NAME}" sh -c 'touch /usr/local/bin/x' 2>/dev/null; then
    echo "FAIL: the root filesystem is writable" >&2
    exit 1
fi
api="http://127.0.0.1:${PORT}/rest"
auth="u=admin&p=image-test-pw&v=1.16.1&c=image-test&f=json"
curl -fsS "${api}/ping.view?${auth}" | grep -q '"status":"ok"' || { echo "FAIL: ping" >&2; exit 1; }
for _ in $(seq 1 30); do
    curl -fsS "${api}/search3.view?${auth}&query=tone" | grep -q '"title":"Tone"' && break
    sleep 1
done
id="$(curl -fsS "${api}/search3.view?${auth}&query=tone" | grep -o '"id":"tr-[0-9a-f]*"' | head -1 | cut -d'"' -f4)"
[[ -n "${id}" ]] || { echo "FAIL: the scan found no track" >&2; exit 1; }
bytes="$(curl -fsS "${api}/stream.view?${auth}&id=${id}&format=mp3&maxBitRate=128" | wc -c)"
(( bytes > 50000 )) || { echo "FAIL: transcoded stream was ${bytes} bytes" >&2; exit 1; }
podman logs "${NAME}" 2>&1 | grep -q "scan finished: 1 tracks" || { echo "FAIL: no scan log" >&2; exit 1; }
# SIGTERM must stop it promptly (PID 1 ignores unhandled signals).
started="$(date +%s)"
podman stop -t 20 "${NAME}" >/dev/null
(( $(date +%s) - started < 5 )) || { echo "FAIL: SIGTERM did not stop the server" >&2; exit 1; }
podman logs "${NAME}" 2>&1 | grep -q "stopping" || { echo "FAIL: no clean stop" >&2; exit 1; }
podman rm -f "${NAME}" >/dev/null

echo "== end-to-end tests inside the image"
./scripts/toolbox.sh env MS_OPENSSL_STATIC=1 \
    RUSTFLAGS="-C target-feature=+crt-static -C relocation-model=pie" \
    cargo test --release --locked --offline --no-run -q \
    --target x86_64-unknown-linux-musl --target-dir target/image-tests \
    -p ms-web --test subsonic
mkdir -p "${WORK}/tests"
bin="$(find target/image-tests/x86_64-unknown-linux-musl/release/deps -maxdepth 1 -type f \
    -name "subsonic-*" ! -name '*.d' -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2)"
[[ -n "${bin}" ]] || { echo "FAIL: no subsonic test binary" >&2; exit 1; }
cp "${bin}" "${WORK}/tests/subsonic"
chmod -R a+rX "${WORK}/tests"
podman run --rm "${SECURITY[@]}" -e MS_REQUIRE_FFMPEG=1 -v "${WORK}/tests:/tests:ro,Z" \
    --entrypoint /tests/subsonic "${IMAGE}" --test-threads=1 -q
echo "Image checks passed."
