# syntax=docker/dockerfile:1.7
#
# The default image is built from this checkout.  It intentionally does not
# download a mutable binary from an unrelated registry or repository.

ARG RUST_VERSION=1.88.0
# Multi-architecture manifest digests are reviewed release inputs. Dependabot
# proposes digest updates without silently floating either base image.
ARG RUST_IMAGE=rust:1.88.0-bullseye@sha256:b315f988b86912bafa7afd39a6ded0a497bf850ec36578ca9a3bdd6a14d5db4e
ARG DEBIAN_IMAGE=debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

FROM ${RUST_IMAGE} AS builder

ARG RUST_VERSION
ARG VALE_BUILD_COMMIT=dev
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}

# Use the security origin directly: the CDN can cache a false 404 for APT's
# percent-encoded package versions. APT still verifies signed indexes/hashes.
RUN sed -i 's|http://deb.debian.org/debian-security|https://security.debian.org/debian-security|g' /etc/apt/sources.list \
    && apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates \
        binutils \
        clang \
        cmake \
        git \
        golang-go \
        libclang-dev \
        nasm \
        perl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Build dependencies in their own layer where possible.
COPY Cargo.toml Cargo.lock README.md rust-toolchain.toml ./
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release --locked --bin redlib \
    && rm -rf src

COPY . ./
RUN VALE_BUILD_COMMIT="$VALE_BUILD_COMMIT" cargo build --release --locked --bin redlib \
    && strip --strip-all target/release/redlib \
    && install -D -m 0755 target/release/redlib /out/vale

FROM ${DEBIAN_IMAGE} AS runtime

ARG VALE_BUILD_COMMIT=dev

RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates \
        curl \
        ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 vale \
    && useradd --system --uid 10001 --gid vale --home-dir /var/lib/vale \
        --no-create-home --shell /usr/sbin/nologin vale \
    && install -d -o vale -g vale -m 0700 \
        /var/lib/vale /var/lib/vale/archives \
        /var/cache/vale /var/cache/vale/video-downloads

COPY --from=builder /out/vale /usr/local/bin/vale
COPY LICENSE CREDITS THIRD_PARTY.md THIRD_PARTY_LICENSES.html /usr/share/doc/vale/
COPY static/hls.LICENSE.txt /usr/share/doc/vale/static/hls.LICENSE.txt
COPY static/fonts/OFL.txt /usr/share/doc/vale/static/fonts/OFL.txt
RUN printf '%s\n' \
      "This Vale image was built from source revision $VALE_BUILD_COMMIT." \
      "Complete corresponding source: https://github.com/Dankpaws/vale" \
      "Vale and its Redlib-derived source are licensed under AGPL-3.0-only." \
      > /usr/share/doc/vale/SOURCE_OFFER.txt

LABEL org.opencontainers.image.title="Vale" \
      org.opencontainers.image.description="A private, subscription-first Reddit reader" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.source="https://github.com/Dankpaws/vale" \
      org.opencontainers.image.url="https://github.com/Dankpaws/vale" \
      org.opencontainers.image.revision="$VALE_BUILD_COMMIT"

ENV REDLIB_ADDRESS=0.0.0.0 \
    PORT=8080 \
    VALE_PROFILE_MODE=accounts \
    VALE_PROFILE_DATABASE=/var/lib/vale/profiles.sqlite3 \
    VALE_ARCHIVE_DIR=/var/lib/vale/archives \
    VALE_MEDIA_CACHE_DIR=/var/cache/vale \
    VALE_ARCHIVE_ITEM_MAX_BYTES=1073741824 \
    VALE_ARCHIVE_TOTAL_MAX_BYTES=2147483648 \
    REDLIB_ROBOTS_DISABLE_INDEXING=on \
    RUST_LOG=warn

WORKDIR /var/lib/vale
USER 10001:10001
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/vale"]
