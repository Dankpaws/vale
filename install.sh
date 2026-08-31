#!/usr/bin/env bash
# Install Vale from a source checkout or from a caller-supplied release URL.
#
# This script deliberately has no embedded repository, registry, or hostname.
# A release install therefore requires VALE_RELEASE_BASE_URL (or the matching
# command-line option); running it from a checkout uses that checkout instead.
set -Eeuo pipefail
IFS=$'\n\t'
umask 077

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
source_dir="${VALE_SOURCE_DIR:-}"
release_base_url="${VALE_RELEASE_BASE_URL:-}"
artifact_url="${VALE_ARTIFACT_URL:-}"
checksum_url="${VALE_CHECKSUM_URL:-}"
github_repository="${VALE_GITHUB_REPOSITORY:-}"
bind_address="${VALE_BIND_ADDRESS:-127.0.0.1}"
port="${VALE_PORT:-8080}"
cookie_secure="${VALE_COOKIE_SECURE:-off}"
public_url="${VALE_PUBLIC_URL:-}"
bind_address_requested=0
port_requested=0
cookie_secure_requested=0
public_url_requested=0
[[ -v VALE_BIND_ADDRESS ]] && bind_address_requested=1
[[ -v VALE_PORT ]] && port_requested=1
[[ -v VALE_COOKIE_SECURE ]] && cookie_secure_requested=1
[[ -v VALE_PUBLIC_URL ]] && public_url_requested=1
skip_dependencies="${VALE_SKIP_DEPENDENCIES:-0}"
no_start=0
work_dir=""

die() {
  printf 'Vale install error: %s\n' "$*" >&2
  exit 1
}

info() {
  printf '%s\n' "$*"
}

usage() {
  cat <<'USAGE'
Usage: sudo ./install.sh [options]

Install the Vale binary and its systemd service on Linux.  When run from the
source checkout, the binary is built locally.  Otherwise provide an exact
HTTPS release directory with VALE_RELEASE_BASE_URL or --release-base-url.

Options:
  --source PATH              Build from this source checkout
  --release-base-url URL     Directory containing vale-<target>.tar.gz assets
  --artifact-url URL         Explicit archive URL (overrides the base URL)
  --checksum-url URL         Explicit SHA-256 URL (overrides the archive URL)
  --github-repository OWNER/REPO
                             Require GitHub/Sigstore provenance verification
  --bind-address ADDRESS     Listener address (default: 127.0.0.1)
  --port PORT                Listener port (default: 8080)
  --cookie-secure on|off     Secure session-cookie flag (default: off)
  --public-url URL           Exact external HTTPS origin for remote access
  --skip-dependencies        Do not install optional ffmpeg video support
  --no-start                 Install and enable the unit without starting it
  -h, --help                 Show this help

Listener and origin options configure only a first installation.  When
/etc/vale/vale.env exists, the installer preserves it and rejects those
options; edit that protected file deliberately for a reconfiguration.

The installer never asks for or stores an owner password.  Create the first
account in the browser at /setup after the service is healthy.
USAGE
}

while (($# > 0)); do
  case "$1" in
    --source)
      (($# >= 2)) || die "--source requires a path"
      source_dir="$2"
      shift 2
      ;;
    --release-base-url)
      (($# >= 2)) || die "--release-base-url requires a URL"
      release_base_url="$2"
      shift 2
      ;;
    --artifact-url)
      (($# >= 2)) || die "--artifact-url requires a URL"
      artifact_url="$2"
      shift 2
      ;;
    --checksum-url)
      (($# >= 2)) || die "--checksum-url requires a URL"
      checksum_url="$2"
      shift 2
      ;;
    --github-repository)
      (($# >= 2)) || die "--github-repository requires OWNER/REPO"
      github_repository="$2"
      shift 2
      ;;
    --bind-address)
      (($# >= 2)) || die "--bind-address requires an address"
      bind_address="$2"
      bind_address_requested=1
      shift 2
      ;;
    --port)
      (($# >= 2)) || die "--port requires a number"
      port="$2"
      port_requested=1
      shift 2
      ;;
    --cookie-secure)
      (($# >= 2)) || die "--cookie-secure requires on or off"
      cookie_secure="$2"
      cookie_secure_requested=1
      shift 2
      ;;
    --public-url)
      (($# >= 2)) || die "--public-url requires an origin URL"
      public_url="$2"
      public_url_requested=1
      shift 2
      ;;
    --skip-dependencies)
      skip_dependencies=1
      shift
      ;;
    --no-start)
      no_start=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1 (use --help for usage)"
      ;;
  esac
done

[[ "$(uname -s)" == "Linux" ]] || die "native installation is supported on Linux only"
(( EUID == 0 )) || die "run as root (for example: sudo $0 ...)"

command -v systemctl >/dev/null 2>&1 || die "systemd is required; use Docker Compose on non-systemd hosts"
[[ -d /run/systemd/system ]] || die "systemd is not running; use Docker Compose on this host"
command -v flock >/dev/null 2>&1 || die "flock is required to serialize Vale installations"
exec 9>/run/lock/vale-install.lock
flock --nonblock 9 || die "another Vale installation is already running"

[[ "$bind_address" =~ ^[0-9A-Fa-f:.]+$ ]] || die "bind address must be a numeric IPv4 or IPv6 address"
[[ "$port" =~ ^[0-9]+$ ]] || die "port must be a number from 1 to 65535"
port_number=$((10#$port))
(( port_number >= 1 && port_number <= 65535 )) || die "port must be a number from 1 to 65535"
case "$cookie_secure" in
  on|off) ;;
  *) die "--cookie-secure must be on or off" ;;
esac

validate_public_url() {
  local value="$1"
  [[ "$value" == https://* ]] || return 1
  [[ "$value" != *[[:space:]]* ]] || return 1
  local authority="${value#https://}"
  [[ "$authority" =~ ^(\[[0-9A-Fa-f:]+\]|[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?)(:[0-9]{1,5})?$ ]]
}

if [[ -n "$public_url" ]] && ! validate_public_url "$public_url"; then
  die "--public-url must be an exact HTTPS origin with no path"
fi

environment_file=/etc/vale/vale.env
if [[ -e "$environment_file" || -L "$environment_file" ]]; then
  [[ -f "$environment_file" && ! -L "$environment_file" ]] || \
    die "existing configuration is not a regular file: $environment_file"
  [[ -r "$environment_file" ]] || \
    die "existing configuration is not readable: $environment_file"
fi
if [[ -e "$environment_file" ]] && (( bind_address_requested || port_requested || cookie_secure_requested || public_url_requested )); then
  die "listener and origin options are first-install only when $environment_file already exists; edit that protected file deliberately, then restart and verify vale.service"
fi
configured_port="$port_number"
configured_address="$bind_address"
configured_cookie_secure="$cookie_secure"
configured_public_url="$public_url"
if [[ -r "$environment_file" ]]; then
  read_configured_port="$(awk -F= '$1 == "PORT" { print $2; exit }' "$environment_file")"
  if [[ -n "$read_configured_port" ]]; then
    [[ "$read_configured_port" =~ ^[0-9]+$ ]] || die "existing PORT in $environment_file is invalid"
    configured_port=$((10#$read_configured_port))
    (( configured_port >= 1 && configured_port <= 65535 )) || die "existing PORT in $environment_file is out of range"
  fi
  read_configured_address="$(awk -F= '$1 == "REDLIB_ADDRESS" { print $2; exit }' "$environment_file")"
  if [[ -n "$read_configured_address" ]]; then
    [[ "$read_configured_address" =~ ^[0-9A-Fa-f:.]+$ ]] || die "existing REDLIB_ADDRESS in $environment_file is invalid"
    configured_address="$read_configured_address"
  fi
  read_configured_cookie="$(awk -F= '$1 == "VALE_COOKIE_SECURE" { print $2; exit }' "$environment_file")"
  if [[ -n "$read_configured_cookie" ]]; then
    case "$read_configured_cookie" in
      on|off) configured_cookie_secure="$read_configured_cookie" ;;
      *) die "existing VALE_COOKIE_SECURE in $environment_file is invalid" ;;
    esac
  fi
  read_configured_public_url="$(awk -F= '$1 == "REDLIB_FULL_URL" { sub(/^[^=]*=/, ""); print; exit }' "$environment_file")"
  if [[ -n "$read_configured_public_url" ]]; then
    configured_public_url="$read_configured_public_url"
  fi
fi

case "$configured_address" in
  127.*|::1) ;;
  *)
    [[ "$configured_cookie_secure" == on ]] || \
      die "a non-loopback listener requires VALE_COOKIE_SECURE=on"
    validate_public_url "$configured_public_url" || \
      die "a non-loopback listener requires an exact HTTPS --public-url with no path"
    ;;
esac

managed_directories=(
  /usr/local/lib/vale
  /etc/vale
  /var/lib/vale
  /var/lib/vale/archives
  /var/cache/vale
  /var/cache/vale/video-downloads
)

validate_managed_directories() {
  local managed_directory
  for managed_directory in "${managed_directories[@]}"; do
    if [[ -L "$managed_directory" || ( -e "$managed_directory" && ! -d "$managed_directory" ) ]]; then
      die "managed Vale path is not a regular directory: $managed_directory"
    fi
  done
}

# Fail before downloading or building when an existing installation has an
# invalid managed path. The same check runs again after the old service has
# stopped and immediately before the installer changes any runtime control.
validate_managed_directories

case "$(uname -m)" in
  x86_64|amd64)
    target="x86_64-unknown-linux-gnu.2.31"
    ;;
  aarch64|arm64)
    target="aarch64-unknown-linux-gnu.2.31"
    ;;
  *)
    die "unsupported architecture $(uname -m); supported architectures are x86_64 and aarch64"
    ;;
esac

if [[ -z "$source_dir" && -z "$artifact_url" && -z "$release_base_url" && -f "$script_dir/Cargo.toml" ]]; then
  source_dir="$script_dir"
fi
if [[ -n "$source_dir" ]]; then
  source_dir="$(cd -- "$source_dir" 2>/dev/null && pwd -P)" || die "source path is not accessible"
  [[ -f "$source_dir/Cargo.toml" ]] || die "source path does not contain Cargo.toml: $source_dir"
fi

work_dir="$(mktemp -d /var/tmp/vale-install.XXXXXX)"
transaction_started=0
transaction_committed=0
rollback_in_progress=0
preserve_work_dir=0

cleanup() {
  if (( preserve_work_dir == 0 )) && [[ -n "${work_dir:-}" && -d "$work_dir" ]]; then
    rm -rf -- "$work_dir"
  fi
}

finish() {
  local status=$?
  trap - EXIT
  trap '' INT TERM
  if (( status != 0 && transaction_started && ! transaction_committed )); then
    rollback_install || true
  fi
  cleanup
  exit "$status"
}

trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

download() {
  local url="$1"
  local destination="$2"
  [[ "$url" == https://* ]] || die "release URLs must use HTTPS"
  if command -v curl >/dev/null 2>&1; then
    curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
      --output "$destination" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget --https-only --quiet --output-document="$destination" "$url"
  else
    die "curl or wget is required to download a release"
  fi
}

verify_sha256() {
  local archive="$1"
  local checksum_file="$2"
  local expected actual
  expected="$(awk 'NF { print $1; exit }' "$checksum_file")"
  [[ "$expected" =~ ^[[:xdigit:]]{64}$ ]] || die "checksum file does not contain a SHA-256 digest"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$archive" | awk '{ print $1 }')"
  else
    command -v shasum >/dev/null 2>&1 || die "sha256sum or shasum is required to verify releases"
    actual="$(shasum -a 256 "$archive" | awk '{ print $1 }')"
  fi
  [[ "${actual,,}" == "${expected,,}" ]] || die "release checksum verification failed"
}

verify_github_attestation() {
  local archive="$1"
  local repository="$2"
  [[ "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || \
    die "--github-repository must use the OWNER/REPO form"
  command -v gh >/dev/null 2>&1 || \
    die "GitHub CLI is required when --github-repository is set"
  info "Verifying GitHub/Sigstore build provenance for $repository"
  gh attestation verify "$archive" --repo "$repository" >/dev/null || \
    die "GitHub build-provenance verification failed"
}

binary_path=""
if [[ -n "$source_dir" ]]; then
  info "Building Vale from $source_dir"
  cargo_path=""
  build_user=""
  build_home=""
  if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != root ]]; then
    build_home="$(getent passwd "$SUDO_USER" | awk -F: 'NR == 1 { print $6 }')"
    if [[ -n "$build_home" && -x "$build_home/.cargo/bin/cargo" ]]; then
      command -v runuser >/dev/null 2>&1 || die "runuser is required to build with the invoking user's Rust toolchain"
      cargo_path="$build_home/.cargo/bin/cargo"
      build_user="$SUDO_USER"
    fi
  fi
  if [[ -n "$build_user" ]]; then
    chown "$build_user" "$work_dir"
    runuser --user "$build_user" -- env -i \
      HOME="$build_home" USER="$build_user" LOGNAME="$build_user" \
      PATH="$build_home/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
      CARGO_HOME="$build_home/.cargo" RUSTUP_HOME="$build_home/.rustup" \
      CARGO_TARGET_DIR="$work_dir/target" \
      "$cargo_path" build --release --locked --bin redlib \
        --manifest-path "$source_dir/Cargo.toml"
  else
    command -v cargo >/dev/null 2>&1 || die "cargo is required for a source-checkout install"
    CARGO_TARGET_DIR="$work_dir/target" cargo build --release --locked --bin redlib \
      --manifest-path "$source_dir/Cargo.toml"
  fi
  binary_path="$work_dir/target/release/redlib"
  [[ -x "$binary_path" ]] || die "cargo did not produce target/release/redlib"
else
  if [[ -z "$artifact_url" ]]; then
    [[ -n "$release_base_url" ]] || die "run from a source checkout or set VALE_RELEASE_BASE_URL"
    artifact_name="vale-${target}.tar.gz"
    artifact_url="${release_base_url%/}/$artifact_name"
  fi
  [[ -n "$checksum_url" ]] || checksum_url="${artifact_url}.sha256"
  archive_path="$work_dir/archive.tar.gz"
  checksum_path="$work_dir/archive.tar.gz.sha256"
  info "Downloading Vale release for $target"
  download "$artifact_url" "$archive_path"
  download "$checksum_url" "$checksum_path"
  verify_sha256 "$archive_path" "$checksum_path"
  if [[ -n "$github_repository" ]]; then
    verify_github_attestation "$archive_path" "$github_repository"
  fi
  archive_listing="$(tar --list --gzip --file="$archive_path")" || die "release archive could not be inspected"
  [[ -n "$archive_listing" ]] || die "release archive is empty"
  invalid_member=0
  vale_members=0
  while IFS= read -r member; do
    case "$member" in
      vale) ((vale_members += 1)) ;;
      LICENSE|CREDITS|THIRD_PARTY.md|THIRD_PARTY_LICENSES.html|SOURCE_OFFER.txt|static/hls.LICENSE.txt|static/fonts/OFL.txt) ;;
      *) invalid_member=1 ;;
    esac
  done <<<"$archive_listing"
  (( invalid_member == 0 && vale_members == 1 )) || \
    die "release archive contains an unexpected or duplicate path"
  [[ -z "$(printf '%s\n' "$archive_listing" | sort | uniq -d)" ]] || \
    die "release archive contains duplicate paths"
  mkdir "$work_dir/unpack"
  tar --extract --gzip --file="$archive_path" --directory="$work_dir/unpack" \
    --no-same-owner --no-same-permissions
  binary_path="$work_dir/unpack/vale"
  [[ -f "$binary_path" && ! -L "$binary_path" && -x "$binary_path" ]] || \
    die "release archive does not contain an executable Vale binary"
fi

if command -v file >/dev/null 2>&1; then
  binary_description="$(file -b "$binary_path")"
  case "$binary_description" in
    *ELF*) ;;
    *) die "the selected release is not a Linux ELF binary" ;;
  esac
fi
binary_version="$("$binary_path" --version 2>/dev/null)" || die "the selected Vale binary cannot run on this host"
info "Using $binary_version for target $target"

install_ffmpeg() {
  command -v ffmpeg >/dev/null 2>&1 && return 0
  if [[ "$skip_dependencies" == 1 ]]; then
    info "Skipping optional ffmpeg installation; video downloads will be unavailable."
    return 0
  fi
  info "Installing ffmpeg with the host package manager"
  if command -v apt-get >/dev/null 2>&1; then
    if apt-get update; then
      apt-get install --yes ffmpeg || true
    fi
  elif command -v dnf >/dev/null 2>&1; then
    dnf install --assumeyes ffmpeg || true
  elif command -v apk >/dev/null 2>&1; then
    apk add ffmpeg || true
  elif command -v pacman >/dev/null 2>&1; then
    pacman --sync --needed --noconfirm ffmpeg || true
  else
    info "No supported package manager was found; continuing without optional ffmpeg video downloads."
    return 0
  fi
  if ! command -v ffmpeg >/dev/null 2>&1; then
    info "FFmpeg is still unavailable; continuing without video-download assembly."
  fi
}
install_ffmpeg

if ! getent group vale >/dev/null 2>&1; then
  groupadd --system vale
fi
if ! getent passwd vale >/dev/null 2>&1; then
  useradd --system --gid vale --home-dir /var/lib/vale --no-create-home \
    --shell /usr/sbin/nologin vale
fi
command -v runuser >/dev/null 2>&1 || \
  die "runuser is required to create Vale state as the unprivileged service user"

binary_destination=/usr/local/lib/vale/vale
admin_destination=/usr/local/bin/vale-admin
service_destination=/etc/systemd/system/vale.service
binary_backup="$work_dir/vale.previous"
admin_backup="$work_dir/vale-admin.previous"
service_backup="$work_dir/vale.service.previous"
binary_staging=/usr/local/lib/vale/.vale.new.$$
had_previous_admin=0
had_previous_service=0
had_previous_binary=0
service_was_active=0
service_was_enabled=0
service_was_runtime_enabled=0
needs_environment=0

admin_staging="$work_dir/vale-admin"
admin_source="$script_dir/contrib/vale-admin"
if [[ -f "$admin_source" ]]; then
  install -m 0755 "$admin_source" "$admin_staging"
else
  cat >"$admin_staging" <<'ADMIN'
#!/bin/sh
set -eu
if [ -r /etc/vale/vale.env ]; then
  set -a
  . /etc/vale/vale.env
  set +a
fi
if [ "$(id -u)" -eq 0 ]; then
  command -v runuser >/dev/null 2>&1 || {
    echo "runuser is required to preserve Vale state ownership" >&2
    exit 1
  }
  exec runuser --preserve-environment --user vale -- /usr/local/lib/vale/vale "$@"
fi
exec /usr/local/lib/vale/vale "$@"
ADMIN
  chmod 0755 "$admin_staging"
fi

environment_staging="$work_dir/vale.env"
if [[ ! -e "$environment_file" ]]; then
  needs_environment=1
  cat >"$environment_staging" <<EOF
# Generated by Vale install.sh.  Do not put passwords or tokens in this file.
REDLIB_ADDRESS=$bind_address
PORT=$port_number
RUST_LOG=warn
VALE_PROFILE_MODE=accounts
VALE_PROFILE_DATABASE=/var/lib/vale/profiles.sqlite3
VALE_ARCHIVE_DIR=/var/lib/vale/archives
VALE_MEDIA_CACHE_DIR=/var/cache/vale
VALE_ARCHIVE_ITEM_MAX_BYTES=1073741824
VALE_ARCHIVE_TOTAL_MAX_BYTES=2147483648
VALE_SESSION_DAYS=30
VALE_COOKIE_SECURE=$cookie_secure
REDLIB_FULL_URL=$public_url
REDLIB_ROBOTS_DISABLE_INDEXING=on
EOF
fi

service_staging="$work_dir/vale.service"
service_source="$script_dir/contrib/vale.service"
if [[ -f "$service_source" ]]; then
  install -m 0644 "$service_source" "$service_staging"
else
  cat >"$service_staging" <<'UNIT'
[Unit]
Description=Vale personal Reddit reader
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=vale
Group=vale
WorkingDirectory=/var/lib/vale
EnvironmentFile=-/etc/vale/vale.env
ExecStart=/usr/local/lib/vale/vale
Restart=on-failure
RestartSec=5s
TimeoutStartSec=120s
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
PrivateDevices=true
ProtectSystem=strict
ProtectHome=true
ProtectClock=true
ProtectHostname=true
ProtectKernelLogs=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
LockPersonality=true
RestrictNamespaces=true
RestrictSUIDSGID=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
CapabilityBoundingSet=
AmbientCapabilities=
ReadWritePaths=/var/lib/vale /var/cache/vale
ReadOnlyPaths=/usr/local/lib/vale
LimitNOFILE=8192

[Install]
WantedBy=multi-user.target
UNIT
fi

if [[ -e "$binary_destination" || -L "$binary_destination" ]]; then
  [[ -f "$binary_destination" && ! -L "$binary_destination" && -x "$binary_destination" ]] || \
    die "existing Vale binary is not a regular executable file: $binary_destination"
  had_previous_binary=1
  install -m 0755 "$binary_destination" "$binary_backup"
fi
if [[ -e "$admin_destination" || -L "$admin_destination" ]]; then
  [[ -f "$admin_destination" && ! -L "$admin_destination" ]] || \
    die "existing Vale admin helper is not a regular file: $admin_destination"
  had_previous_admin=1
  install -m 0755 "$admin_destination" "$admin_backup"
fi
if [[ -e "$service_destination" || -L "$service_destination" ]]; then
  [[ -f "$service_destination" && ! -L "$service_destination" ]] || \
    die "existing Vale service unit is not a regular file: $service_destination"
  had_previous_service=1
  install -m 0644 "$service_destination" "$service_backup"
fi

service_fragment="$(systemctl show vale.service --property=FragmentPath --value 2>/dev/null || true)"
if [[ -n "$service_fragment" && "$service_fragment" != "$service_destination" ]]; then
  die "vale.service is loaded from an unmanaged path: $service_fragment"
fi
service_active_state="$(systemctl show vale.service --property=ActiveState --value 2>/dev/null || true)"
case "$service_active_state" in
  active) service_was_active=1 ;;
  failed|inactive|"") ;;
  *) die "vale.service is in a transitional state: $service_active_state" ;;
esac
service_enable_state="$(systemctl is-enabled vale.service 2>/dev/null || true)"
case "$service_enable_state" in
  enabled) service_was_enabled=1 ;;
  enabled-runtime)
    service_was_enabled=1
    service_was_runtime_enabled=1
    ;;
  disabled|not-found|"") ;;
  masked|masked-runtime)
    die "vale.service is masked; unmask it deliberately before installing"
    ;;
  *)
    die "vale.service has an unsupported enable state: $service_enable_state"
    ;;
esac

{
  printf 'Vale installer recovery record\n'
  printf 'selected_version=%s\n' "$binary_version"
  printf 'previous_binary=%s\n' "$had_previous_binary"
  printf 'previous_admin_helper=%s\n' "$had_previous_admin"
  printf 'previous_service_unit=%s\n' "$had_previous_service"
  printf 'previous_service_active=%s\n' "$service_was_active"
  printf 'previous_service_enable_state=%s\n' "${service_enable_state:-not-found}"
} >"$work_dir/recovery.txt"
chmod 0600 "$work_dir/recovery.txt"

service_stays_active() {
  local check
  for check in 1 2 3; do
    systemctl is-active --quiet vale.service || return 1
    (( check == 3 )) || sleep 1
  done
}

rollback_install() {
  (( rollback_in_progress == 0 )) || return 0
  rollback_in_progress=1
  local rollback_failed=0
  local restored_enable_state=""
  set +e
  info "Vale installation did not complete; restoring the previous runtime controls."

  if [[ -e "$service_destination" ]]; then
    systemctl mask --runtime --now vale.service >/dev/null 2>&1 || rollback_failed=1
    systemctl stop vale.service >/dev/null 2>&1 || rollback_failed=1
  else
    systemctl mask --runtime vale.service >/dev/null 2>&1 || rollback_failed=1
  fi
  systemctl is-active --quiet vale.service && rollback_failed=1
  rm -f -- "$binary_staging"

  # Remove only the two enablement links this unit can create while the unit
  # remains runtime-masked. Restoration below recreates only the captured
  # prior enable state.
  for enable_link in \
    /etc/systemd/system/multi-user.target.wants/vale.service \
    /run/systemd/system/multi-user.target.wants/vale.service; do
    if [[ -L "$enable_link" ]]; then
      rm -f -- "$enable_link" || rollback_failed=1
    elif [[ -e "$enable_link" ]]; then
      rollback_failed=1
    fi
  done

  if (( had_previous_binary )); then
    install -o root -g root -m 0755 "$binary_backup" "$binary_destination" || rollback_failed=1
  else
    rm -f -- "$binary_destination" || rollback_failed=1
  fi
  if (( had_previous_admin )); then
    install -o root -g root -m 0755 "$admin_backup" "$admin_destination" || rollback_failed=1
  else
    rm -f -- "$admin_destination" || rollback_failed=1
  fi
  if (( had_previous_service )); then
    install -o root -g root -m 0644 "$service_backup" "$service_destination" || rollback_failed=1
  else
    rm -f -- "$service_destination" || rollback_failed=1
  fi
  systemctl daemon-reload >/dev/null 2>&1 || rollback_failed=1

  if (( rollback_failed == 0 )); then
    systemctl unmask --runtime vale.service >/dev/null 2>&1 || rollback_failed=1
  fi
  if (( rollback_failed == 0 )); then
    if (( service_was_enabled )); then
      if (( service_was_runtime_enabled )); then
        systemctl enable --runtime vale.service >/dev/null 2>&1 || rollback_failed=1
      else
        systemctl enable vale.service >/dev/null 2>&1 || rollback_failed=1
      fi
    fi
  fi
  if (( rollback_failed == 0 )); then
    restored_enable_state="$(systemctl is-enabled vale.service 2>/dev/null || true)"
    if (( had_previous_service )); then
      [[ "$restored_enable_state" == "${service_enable_state:-disabled}" ]] || rollback_failed=1
    else
      case "$restored_enable_state" in
        not-found|"") ;;
        *) rollback_failed=1 ;;
      esac
    fi
  fi
  if (( rollback_failed == 0 )); then
    if (( service_was_active )); then
      systemctl start vale.service >/dev/null 2>&1 || rollback_failed=1
      (( rollback_failed )) || service_stays_active || rollback_failed=1
    else
      systemctl is-active --quiet vale.service && rollback_failed=1
    fi
  fi

  if (( rollback_failed )); then
    preserve_work_dir=1
    systemctl mask --runtime --now vale.service >/dev/null 2>&1 || true
    printf 'Vale rollback warning: one or more restoration steps failed; keep the service stopped and inspect the root-only recovery files at %s before retrying.\n' "$work_dir" >&2
  else
    info "The previous Vale runtime state was restored. Configuration, profile, and archive data were not changed."
  fi
  return 0
}

# Everything below this line is a runtime-control transaction. A runtime mask
# quiesces an existing unit before the second path check. Root creates or
# normalizes only the state/cache boundary entries whose names are protected by
# the root-owned /var/lib and /var/cache parents; those boundary directories
# must then belong to vale so SQLite and the cache can create direct children.
# Deeper directories are created by the service identity, and root never
# recursively chowns those writable trees.
transaction_started=1
if (( had_previous_service )); then
  systemctl mask --runtime --now vale.service >/dev/null
else
  systemctl mask --runtime vale.service >/dev/null
fi
if systemctl is-active --quiet vale.service; then
  die "vale.service could not be stopped before installation"
fi
case "$(systemctl is-enabled vale.service 2>/dev/null || true)" in
  masked|masked-runtime) ;;
  *) die "vale.service could not be runtime-masked before installation" ;;
esac
validate_managed_directories

install -d -o root -g root -m 0755 /usr/local/lib/vale /etc/vale
install -d -o vale -g vale -m 0700 /var/lib/vale /var/cache/vale
runuser --user vale -- mkdir -p -- /var/lib/vale/archives /var/cache/vale/video-downloads
validate_managed_directories
for service_directory in \
  /var/lib/vale /var/lib/vale/archives \
  /var/cache/vale /var/cache/vale/video-downloads; do
  [[ "$(stat -c '%U:%G' "$service_directory")" == "vale:vale" ]] || \
    die "managed Vale state is not owned by vale:vale: $service_directory"
  runuser --user vale -- chmod 0700 -- "$service_directory"
done

install -o root -g root -m 0755 "$binary_path" "$binary_staging"
mv -f -- "$binary_staging" "$binary_destination"
install -o root -g root -m 0755 "$admin_staging" "$admin_destination"
if (( needs_environment )); then
  install -o root -g vale -m 0640 "$environment_staging" "$environment_file"
  info "Created nonsecret configuration at $environment_file"
else
  info "Preserving existing configuration at $environment_file"
fi
if ! cmp -s "$service_staging" "$service_destination" 2>/dev/null; then
  install -o root -g root -m 0644 "$service_staging" "$service_destination"
fi

systemctl unmask --runtime vale.service >/dev/null
systemctl daemon-reload
systemctl enable vale.service >/dev/null
[[ "$(systemctl is-enabled vale.service 2>/dev/null || true)" == "enabled" ]] || \
  die "vale.service could not be enabled"

case "$configured_address" in
  0.0.0.0|127.*) health_host="127.0.0.1" ;;
  ::|::0|::1) health_host="[::1]" ;;
  *:*) health_host="[$configured_address]" ;;
  *) health_host="$configured_address" ;;
esac

probe_health() {
  if command -v curl >/dev/null 2>&1; then
    curl --fail --silent --show-error --max-time 5 \
      --noproxy '*' "http://${health_host}:${configured_port}/healthz" >/dev/null 2>&1
  else
    wget --quiet --timeout=5 --spider \
      "http://${health_host}:${configured_port}/healthz"
  fi
}

wait_for_health() {
  local _
  for _ in {1..60}; do
    probe_health && return 0
    sleep 1
  done
  return 1
}

if (( no_start )); then
  systemctl is-active --quiet vale.service && die "vale.service started despite --no-start"
  info "Vale is installed and enabled but not started (--no-start)."
else
  command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || \
    die "curl or wget is required for the post-install health check"
  systemctl restart vale.service >/dev/null
  if ! wait_for_health; then
    die "the new Vale service failed its health check"
  fi
  service_stays_active || die "the new Vale service did not remain active after its health check"
  info "Vale service is healthy."
fi
transaction_committed=1

info ""
if [[ -n "$configured_public_url" ]]; then
  display_url="${configured_public_url}/"
else
  display_url="http://${health_host}:${configured_port}/"
fi
info "Vale is ready at $display_url"
info "Open /setup in that browser to create the first account; no password was generated or stored by this installer."
info "Service: vale.service"
info "State: /var/lib/vale   Cache: /var/cache/vale   Config: $environment_file"
info "Interactive recovery: sudo /usr/local/bin/vale-admin admin reset-password --username ACCOUNT"
if [[ "$configured_cookie_secure" == off ]]; then
  info "Local HTTP is enabled.  Set VALE_COOKIE_SECURE=on before using an HTTPS reverse proxy."
fi
