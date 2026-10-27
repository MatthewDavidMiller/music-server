#!/bin/bash
# Run music-server locally for manual testing, at http://127.0.0.1:8080.
#
# Usage: make dev   (or ./scripts/dev-server.sh)
#
# State lives in ./data (git-ignored): a development master key and users
# file generated on first use, the sealed user data, and the caches. The
# music is ./music-dev, or the directory in MS_DEV_MUSIC. Point a Subsonic
# client at http://127.0.0.1:8080 as user "admin", password "dev-password".
# Delete ./data to start over.

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA="${PROJECT_ROOT}/data"
PORT="${MS_DEV_PORT:-8080}"
MUSIC="${MS_DEV_MUSIC:-${PROJECT_ROOT}/music-dev}"

[[ "${MUSIC}" == "${PROJECT_ROOT}"/* ]] || {
    echo "ERROR: MS_DEV_MUSIC must be inside the repository (the toolbox only mounts it)" >&2
    exit 1
}
mkdir -p "${DATA}/state" "${DATA}/cache" "${DATA}/tmp" "${MUSIC}"
chmod 700 "${DATA}" "${DATA}/state"

if [[ ! -s "${DATA}/users.json" ]]; then
    (umask 077 && printf '%s\n' \
        '{"version":1,"users":[{"name":"admin","password":"dev-password","admin":true}]}' \
        > "${DATA}/users.json")
    echo "Wrote a development users file to data/users.json (admin / dev-password)"
fi

export MS_LISTEN="0.0.0.0:${PORT}"
export MS_MUSIC_DIR="${MUSIC}"
export MS_DATA_DIR="${DATA}/state"
export MS_CACHE_DIR="${DATA}/cache"
export MS_TMP_DIR="${DATA}/tmp"
export MS_MASTER_KEY_FILE="${DATA}/dev.key"
export MS_USERS_FILE="${DATA}/users.json"
export MS_SECURE_COOKIES=false
export MS_SCAN_INTERVAL_MINUTES="${MS_SCAN_INTERVAL_MINUTES:-5}"

# shellcheck disable=SC2016 # expanded inside the toolbox, not here
exec "${PROJECT_ROOT}/scripts/toolbox.sh" --publish "${PORT}" bash -c '
    set -euo pipefail
    cargo build -q -p ms-web --bin music-server
    if [[ ! -s "${MS_MASTER_KEY_FILE}" ]]; then
        (umask 077 && target/debug/music-server gen-key > "${MS_MASTER_KEY_FILE}")
        echo "Generated a development master key in data/dev.key"
    fi
    exec target/debug/music-server serve
'
