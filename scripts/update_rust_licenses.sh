#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
PROJECT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
CARGO_ABOUT_VERSION="0.9.2"

command -v cargo-about >/dev/null 2>&1 || {
	printf 'cargo-about %s is required.\n' "$CARGO_ABOUT_VERSION" >&2
	exit 1
}

actual_version="$(cargo-about --version)"
[[ "$actual_version" == "cargo-about $CARGO_ABOUT_VERSION" ]] || {
	printf 'Expected cargo-about %s, found %s.\n' "$CARGO_ABOUT_VERSION" "$actual_version" >&2
	exit 1
}

cargo-about generate \
	--locked \
	--fail \
	--manifest-path "$PROJECT_DIR/Cargo.toml" \
	--config "$PROJECT_DIR/about.toml" \
	"$PROJECT_DIR/about.hbs" \
	--output-file "$PROJECT_DIR/THIRD_PARTY_LICENSES.html"

printf 'Regenerated THIRD_PARTY_LICENSES.html with cargo-about %s.\n' "$CARGO_ABOUT_VERSION"
