# syntax=docker/dockerfile:1.7
#
# The default image is built from this checkout.  It intentionally does not
# download a mutable binary from an unrelated registry or repository.

ARG RUST_VERSION=1.98.0
# Multi-architecture manifest digests are reviewed release inputs. Dependabot
# proposes digest updates without silently floating either base image.
ARG RUST_IMAGE=rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
ARG DEBIAN_IMAGE=debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

FROM ${RUST_IMAGE} AS builder

ARG RUST_VERSION
ARG VALE_BUILD_COMMIT=dev
# Release binaries keep the documented glibc 2.31 baseline (Debian 11,
# Ubuntu 20.04, RHEL 9) although the builder runs a supported Debian release:
# cargo-zigbuild links against glibc 2.31 symbol versions with Zig's linker.
ARG GLIBC_BASELINE=2.31
ARG ZIG_WHEEL_VERSION=0.13.0.post1
ARG ZIG_WHEEL_SHA256_AMD64=5ff0f2cb0d583b4f94820909612043f62e4d1d9a8bed77ceec088a01fc2ef015
ARG ZIG_WHEEL_SHA256_ARM64=b2ab883f27e915bc34266ff6627f565b5cc7b3c566b5a91f633b4174d52119a9
ARG CARGO_ZIGBUILD_VERSION=0.23.4
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}

RUN apt-get update \
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
        python3 \
    && rm -rf /var/lib/apt/lists/*

# Zig comes from its hash-pinned PyPI wheel (a zip holding the official
# release), which covers both amd64 and arm64 from one reviewed source.
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) wheel_tag=manylinux_2_12_x86_64.manylinux2010_x86_64.musllinux_1_1_x86_64; digest=$ZIG_WHEEL_SHA256_AMD64; path=e1/15/88cd364004181c9db4e4b295df2184edbaed1433226cb315c2e058c00c4a ;; \
        aarch64) wheel_tag=manylinux_2_17_aarch64.manylinux2014_aarch64.musllinux_1_1_aarch64; digest=$ZIG_WHEEL_SHA256_ARM64; path=49/a3/7b0998dee834ef22c6c72c4e3c5c4d7dde06a990acf10ab2db5f59d1d31c ;; \
        *) echo "unsupported build architecture $(uname -m)" >&2; exit 1 ;; \
    esac; \
    wheel="ziglang-${ZIG_WHEEL_VERSION}-py3-none-${wheel_tag}.whl"; \
    curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --output "/tmp/$wheel" "https://files.pythonhosted.org/packages/$path/$wheel"; \
    echo "$digest  /tmp/$wheel" | sha256sum --check --strict; \
    python3 -m zipfile -e "/tmp/$wheel" /opt/zig-wheel; \
    chmod 0755 /opt/zig-wheel/ziglang/zig; \
    ln -s /opt/zig-wheel/ziglang/zig /usr/local/bin/zig; \
    rm "/tmp/$wheel"; \
    zig version; \
    cargo install --locked --version "$CARGO_ZIGBUILD_VERSION" cargo-zigbuild

WORKDIR /src

# Build dependencies in their own layer where possible.
COPY Cargo.toml Cargo.lock README.md rust-toolchain.toml ./
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo zigbuild --release --locked --bin redlib --target "$(uname -m)-unknown-linux-gnu.${GLIBC_BASELINE}" \
    && rm -rf src

COPY . ./
RUN VALE_BUILD_COMMIT="$VALE_BUILD_COMMIT" cargo zigbuild --release --locked --bin redlib --target "$(uname -m)-unknown-linux-gnu.${GLIBC_BASELINE}" \
    && binary="target/$(uname -m)-unknown-linux-gnu/release/redlib" \
    && newest="$(objdump -T "$binary" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n 1)" \
    && [ "$(printf '%s\n' "$newest" "$GLIBC_BASELINE" | sort -V | tail -n 1)" = "$GLIBC_BASELINE" ] \
    && strip --strip-all "$binary" \
    && install -D -m 0755 "$binary" /out/vale

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
