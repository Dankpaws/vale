#!/usr/bin/env bash
set -euo pipefail

# Update these values together after reviewing a stable upstream release.
HLS_VERSION="1.6.17"
HLS_TARBALL_SHA256="4e24999b4021b58ca3ed861c8de89f498e187b016f236d230e014f7b7c8aaa9e"
HLS_TARBALL_URL="https://registry.npmjs.org/hls.js/-/hls.js-${HLS_VERSION}.tgz"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vale-hls.XXXXXX")"
trap 'rm -rf -- "$TEMP_DIR"' EXIT

curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
	"$HLS_TARBALL_URL" --output "$TEMP_DIR/hls.tgz"

if command -v sha256sum >/dev/null 2>&1; then
	ACTUAL_SHA256="$(sha256sum "$TEMP_DIR/hls.tgz" | awk '{print $1}')"
else
	ACTUAL_SHA256="$(shasum -a 256 "$TEMP_DIR/hls.tgz" | awk '{print $1}')"
fi
if [[ "$ACTUAL_SHA256" != "$HLS_TARBALL_SHA256" ]]; then
	printf 'hls.js archive checksum mismatch: expected %s, got %s\n' "$HLS_TARBALL_SHA256" "$ACTUAL_SHA256" >&2
	exit 1
fi

tar -xzf "$TEMP_DIR/hls.tgz" -C "$TEMP_DIR" package/dist/hls.min.js package/LICENSE
{
	printf '%s\n' '// @license http://www.apache.org/licenses/LICENSE-2.0 Apache-2.0'
	printf '// @source  https://github.com/video-dev/hls.js/tree/v%s\n' "$HLS_VERSION"
	cat "$TEMP_DIR/package/dist/hls.min.js"
} > "$TEMP_DIR/hls.min.js"
mv "$TEMP_DIR/hls.min.js" "$SCRIPT_DIR/../static/hls.min.js"
mv "$TEMP_DIR/package/LICENSE" "$SCRIPT_DIR/../static/hls.LICENSE.txt"

printf 'Updated static/hls.min.js and its license to verified hls.js v%s.\n' "$HLS_VERSION"
