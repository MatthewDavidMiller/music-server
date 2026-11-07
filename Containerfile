# The music-server runtime image.
#
# Both stages start from the official Alpine image pinned by digest -- the
# same release and digest as the toolbox (tools/toolbox/Dockerfile) -- so the
# binary is built against the musl and libcrypto it ships with. The builder
# installs the repository-pinned Rust toolchain through the same
# checksum-verified installer the gates use, rather than trusting a second
# base image for the compiler.
#
# The binary is fully static (musl, static-pie, libcrypto linked in), so the
# runtime stage carries no OpenSSL package for it. What the runtime does
# carry is the one program the server runs: ffmpeg, from Alpine, for
# transcoded streams and resized cover art. It is never linked; crates/ms-media
# starts it with a fixed argument vector, a deadline, and a concurrency cap.
#
# Run it read-only, as the non-root user it declares, with the master key and
# the users file as Podman secrets and the music mounted read-only; see
# docs/DEPLOYMENT.md.

ARG ALPINE_DIGEST=sha256:5c216c9767869f6ec7c4718977601d52f087c5c7bf2fdfb198e403f6b48ec40d

FROM docker.io/library/alpine:3.23.8@${ALPINE_DIGEST} AS build

SHELL ["/bin/ash", "-eo", "pipefail", "-c"]

# Upgrade first: the pinned base ships whatever was current when it was
# built, and the static libcrypto linked into the binary should be the
# newest fixed release, not that snapshot.
RUN apk upgrade --no-cache \
    && apk add --no-cache bash build-base curl musl-dev openssl-dev openssl-libs-static

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH="/opt/cargo/bin:${PATH}"
COPY scripts/install-rust.sh /tmp/install-rust.sh
RUN /tmp/install-rust.sh --build-only

WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml LICENSE ./
COPY .cargo .cargo
COPY crates crates

# static-pie against musl with libcrypto linked in; RUSTFLAGS replaces the
# development crt-static=off setting in .cargo/config.toml.
RUN MS_OPENSSL_STATIC=1 \
    RUSTFLAGS="-C target-feature=+crt-static -C relocation-model=pie" \
    cargo build --release --locked --offline --target x86_64-unknown-linux-musl \
        -p ms-web --bin music-server \
    && install -m 0755 target/x86_64-unknown-linux-musl/release/music-server /music-server


FROM docker.io/library/alpine:3.23.8@${ALPINE_DIGEST}

SHELL ["/bin/ash", "-eo", "pipefail", "-c"]

# ffmpeg is the one package added. apk upgrade brings the packages
# preinstalled in the pinned base to their fixed releases; apk add alone
# leaves them at the base image's snapshot, which the lock review's scan
# reports.
RUN apk upgrade --no-cache \
    && apk add --no-cache ffmpeg

COPY --from=build /music-server /usr/local/bin/music-server
COPY crates/ms-web/THIRD_PARTY_NOTICES.txt /usr/share/doc/music-server/THIRD_PARTY_NOTICES.txt

RUN addgroup -S -g 10001 music \
    && adduser -S -D -H -u 10001 -G music -h /var/lib/music-server -s /sbin/nologin music \
    && install -d -m 0700 -o music -g music /var/lib/music-server /var/cache/music-server

ENV PATH="/usr/local/bin:/usr/bin:/bin" \
    MS_LISTEN=0.0.0.0:8080 \
    MS_MUSIC_DIR=/music \
    MS_DATA_DIR=/var/lib/music-server \
    MS_CACHE_DIR=/var/cache/music-server \
    MS_FFMPEG=/usr/bin/ffmpeg

USER 10001:10001
WORKDIR /var/lib/music-server
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["/usr/local/bin/music-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/music-server"]
CMD ["serve"]
