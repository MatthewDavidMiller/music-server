#!/bin/bash
# The Rust gate: formatting, the first-party dependency graph, the unsafe
# allowlist, the toolchain pin, clippy, and the test suites.
#
# Usage: ./scripts/check-rust.sh   (run through scripts/toolbox.sh)

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${PROJECT_ROOT}"

for tool in cargo rustfmt; do
    command -v "${tool}" >/dev/null || { echo "ERROR: ${tool} is required" >&2; exit 1; }
done

echo "== toolchain pin"
pinned="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
installer="$(sed -n 's/^RUST_STABLE="\(.*\)"$/\1/p' scripts/install-rust.sh)"
workspace="$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' Cargo.toml)"
if [[ -z "${pinned}" || "${pinned}" != "${installer}" || "${pinned}" != "${workspace}" ]]; then
    echo "ERROR: rust-toolchain.toml (${pinned}), install-rust.sh (${installer})," \
        "and Cargo.toml rust-version (${workspace}) must name one release" >&2
    exit 1
fi

echo "== rustfmt"
cargo fmt --all -- --check

echo "== workspace-only dependency graph"
cargo generate-lockfile --offline >/dev/null
if grep -q '^source = ' Cargo.lock; then
    echo "ERROR: Cargo.lock names a package from outside the workspace:" >&2
    grep -B2 '^source = ' Cargo.lock >&2
    exit 1
fi
members="$(sed -n 's/^name = "\(.*\)"$/\1/p' Cargo.lock | sort)"
expected="$(find crates -mindepth 2 -maxdepth 2 -name Cargo.toml -exec \
    awk -F'"' '/^\[/ { in_package = ($0 == "[package]") } in_package && /^name = / { print $2 }' {} \; |
    sort)"
if [[ "${members}" != "${expected}" ]]; then
    echo "ERROR: Cargo.lock packages differ from the workspace members" >&2
    diff <(echo "${expected}") <(echo "${members}") >&2 || true
    exit 1
fi

echo "== native library allowlist"
# libcrypto is the one third-party library; pthread is the C library's.
libraries="$(grep -rhoE 'rustc-link-lib=\{kind\}=[A-Za-z0-9_]+|rustc-link-lib=([a-z]+=)?[A-Za-z0-9_]+' crates --include='*.rs' |
    sed 's/.*=//' | sort -u)"
[[ -n "${libraries}" ]] || { echo "ERROR: no linked library found; the pattern no longer matches" >&2; exit 1; }
while IFS= read -r library; do
    [[ "${library}" =~ ^(crypto|pthread)$ ]] || { echo "ERROR: unexpected native library ${library}" >&2; exit 1; }
done <<<"${libraries}"
if grep -rn '#\[link(' crates --include='*.rs' >/dev/null; then
    echo "ERROR: link libraries through crates/ms-sys/build.rs, not #[link]" >&2
    exit 1
fi

echo "== unsafe allowlist"
UNSAFE_CRATES=(ms-sys)
for manifest in crates/*/Cargo.toml; do
    dir="$(dirname "${manifest}")"
    name="$(basename "${dir}")"
    allowed=false
    for a in "${UNSAFE_CRATES[@]}"; do [[ "${name}" == "${a}" ]] && allowed=true; done
    [[ "${allowed}" == true ]] && continue
    while IFS= read -r root; do
        grep -q '^#!\[forbid(unsafe_code)\]' "${root}" || {
            echo "ERROR: ${root} must forbid unsafe_code" >&2
            exit 1
        }
    done < <(find "${dir}/src" -maxdepth 2 \( -name lib.rs -o -name main.rs -o -path '*/bin/*.rs' \))
done

echo "== subprocess allowlist"
# ffmpeg is the one program this server runs, and only ms-media starts it,
# where every argv is fixed and every child is supervised. Integration tests
# may drive ffmpeg themselves to generate fixtures.
SPAWN_CRATES=(ms-media)
spawners="$(grep -rlE 'Command::new\(|process_group\(|pre_exec\(' crates/*/src --include='*.rs' || true)"
while IFS= read -r file; do
    [[ -n "${file}" ]] || continue
    crate="$(echo "${file}" | cut -d/ -f2)"
    allowed=false
    for a in "${SPAWN_CRATES[@]}"; do [[ "${crate}" == "${a}" ]] && allowed=true; done
    # A spawn under a crate's test-only fixture module is not shipped code.
    [[ "${file}" == */testgen.rs || "${file}" == */testutil.rs ]] && allowed=true
    [[ "${allowed}" == true ]] || {
        echo "ERROR: ${file} starts a process; only crates/ms-media may" >&2
        exit 1
    }
done <<<"${spawners}"

echo "== clippy"
cargo clippy --workspace --all-targets --offline --locked -- -D warnings

echo "== tests"
cargo test --workspace --offline --locked
