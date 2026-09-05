# Install Vale

This guide starts from a clean machine. It assumes that you can run commands
as an administrator when installing prerequisites, but it keeps the Vale
process itself unprivileged. Pick one path and finish its **After installation**
checklist before adding more configuration.

## Before you start

You need:

- a 64-bit machine with enough disk space for the binary, profile database,
  and any offline archives;
- outbound HTTPS access to Reddit and the domains used by Reddit media;
- a browser on the same private network as the Vale listener; and
- a plan for HTTPS and a private network boundary if another device will use
  the instance.

Vale does not need a Reddit login. Do not put a Reddit password, OAuth token,
or developer credential in an environment file.

### Release assets and installer trust

Use a tagged release and verify its published checksum and, when possible,
its GitHub/Sigstore build-provenance attestation before running a binary. Do
not replace a pinned release with an unreviewed `latest` download. Vale's
tagged workflow publishes per-archive SHA-256/SHA-512 checksums, a portable
Sigstore bundle, and an attestation in GitHub's artifact-attestation service.
A release that includes the repository-relative installers should provide:

```text
install.sh       # Linux and WSL2 (systemd)
install.ps1      # Windows PowerShell + Docker Desktop
```

Review an installer before running it, and run it from a clean checkout of the
same tagged release. To install a published binary without compiling Rust,
give the script that tag's exact release-asset directory:

```sh
git clone https://github.com/Dankpaws/vale.git
cd vale
git checkout v0.37.0
sudo ./install.sh \
  --release-base-url https://github.com/Dankpaws/vale/releases/download/v0.37.0
```

The checksum detects corruption or a mismatched file, but a checksum fetched
from the same compromised download origin cannot independently authenticate
that origin. For signed provenance verification, install the GitHub CLI and
identify the public repository explicitly:

```sh
sudo ./install.sh \
  --release-base-url https://github.com/Dankpaws/vale/releases/download/v0.37.0 \
  --github-repository Dankpaws/vale
```

That mode fails closed unless `gh attestation verify` confirms the downloaded
archive was produced by the named repository. The installer never silently
downgrades an explicitly requested attestation check.

Vale deliberately retains checksum-only installation for minimal private
hosts that do not have a current GitHub CLI. That compatibility path trusts
the HTTPS release origin and is not independent publisher authentication. It
is an accepted bootstrap tradeoff, not the preferred high-assurance path; the
release maintainer must still verify a published archive's checksum and GitHub
attestation before announcing the release.

The installer must print the selected version, target architecture, install
directories, listener URL, and service name. It must not print passwords,
session cookies, OAuth tokens, or private keys. If your release does not ship
the installer yet, use the source-build fallback for your platform below.

## Linux

### Recommended: release installer

On an x86-64 or ARM64 Linux distribution with systemd and glibc 2.31 or newer,
install `curl` or `wget`, `tar`, and the normal account-management tools
supplied by the distribution.

On Debian or Ubuntu, the prerequisite command is:

```sh
sudo apt update
sudo apt install -y ca-certificates curl git passwd tar
```

On Fedora or another current DNF-based distribution:

```sh
sudo dnf install -y ca-certificates curl git shadow-utils tar
```

On Arch Linux:

```sh
sudo pacman -Syu --needed ca-certificates curl git shadow tar
```

Then check out the tagged release and install it:

```sh
git clone https://github.com/Dankpaws/vale.git
cd vale
git checkout v0.37.0
sudo ./install.sh \
  --release-base-url https://github.com/Dankpaws/vale/releases/download/v0.37.0
```

The script verifies the release checksum (and signed provenance when
`--github-repository` is supplied), defaults to loopback HTTP, creates a
dedicated unprivileged `vale` account, installs `vale.service`, separates
durable state from cache, and waits for `/healthz`. It refuses a non-loopback
listener unless secure cookies and an exact HTTPS origin are configured.
Use the source-build path on an older glibc host; use Docker on a musl-based or
non-systemd distribution.

Video downloads use the current `ffmpeg` package supplied by the host
distribution when it is not already installed. That optional OS package is not
part of the checksummed Vale archive. Use `--skip-dependencies` when package
installation is managed separately; feed reading, accounts, and saved text or
image archives remain available, while video-download assembly is unavailable
until FFmpeg is installed. If the configured package repositories cannot
provide it, the installer warns and continues with those core features.

Before replacing an existing service, the installer stages and verifies the
new controls, records whether the old unit was active and enabled, then
runtime-masks and stops it. Any ordinary error, `Ctrl+C`, or termination signal
after that point restores the previous binary, helper, unit, enablement, and
running state. Configuration, profiles, archives, and cache are preserved. If
restoration itself cannot finish, the unit remains stopped and runtime-masked
and the installer prints an explicit repair warning instead of starting a
partly restored service. That warning names a root-only recovery directory
under `/var/tmp`; retain it until the recorded binary, helper, and unit have
been reconciled, then remove only that exact directory.

### Fallback: build from source

The source build is useful when no matching release asset exists. Vale requires
Rust 1.88 or newer; current stable is recommended. On Debian or Ubuntu,
install the toolchain and native build dependencies first:

```sh
sudo apt update
sudo apt install -y build-essential ca-certificates clang cmake curl git golang-go libclang-dev nasm perl pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

On Fedora, install the matching source prerequisites, then run the same
`rustup` command above:

```sh
sudo dnf group install -y "Development Tools"
sudo dnf install -y ca-certificates clang cmake curl git golang libclang-devel nasm perl pkgconf-pkg-config
```

On Arch Linux, install these packages before running that `rustup` command:

```sh
sudo pacman -Syu --needed base-devel ca-certificates clang cmake curl git go libclang nasm perl pkgconf
```

Clone the release source and build the locked graph:

```sh
git clone https://github.com/Dankpaws/vale.git
cd vale
git checkout v0.37.0
export VALE_CHECKOUT="$(pwd -P)"
cargo build --release --locked --bin redlib
```

To let the installer perform that same source build and install the complete
service contract, run `sudo ./install.sh` from the checkout after installing
the prerequisites above. Under `sudo`, it uses the invoking user's rustup
toolchain rather than placing Rust state in the root account.

The first optimized source build compiles the complete locked dependency graph
and can take several minutes on a small server, including a quiet final link
step. Keep the terminal open while the installer says it is building Vale; a
later tagged-binary install avoids that compilation.

For a local, loopback-only smoke test, create a private working directory and
configuration. The application reads `redlib.toml` from its working directory;
the paths below are examples, not paths that should be copied unchanged onto
every distribution:

```sh
install -d -m 700 "$HOME/.local/share/vale/archives" "$HOME/.cache/vale"
install -d -m 700 "$HOME/.config/vale"
export VALE_PROFILE_DATABASE="$HOME/.local/share/vale/profiles.sqlite3"
export VALE_ARCHIVE_DIR="$HOME/.local/share/vale/archives"
export VALE_MEDIA_CACHE_DIR="$HOME/.cache/vale"
cat > "$HOME/.config/vale/redlib.toml" <<'EOF'
VALE_PROFILE_MODE = "accounts"
VALE_COOKIE_SECURE = "off"
VALE_SESSION_DAYS = "30"
REDLIB_FULL_URL = "http://127.0.0.1:8080"
REDLIB_DEFAULT_THEME = "dark"
REDLIB_DEFAULT_REMOVE_DEFAULT_FEEDS = "on"
REDLIB_ROBOTS_DISABLE_INDEXING = "on"
EOF
```

The exported paths use the current user's home directory and take precedence
over the file. Keep the file mode private:

```sh
chmod 600 "$HOME/.config/vale/redlib.toml"
cd "$HOME/.config/vale"
"$VALE_CHECKOUT/target/release/redlib" --address 127.0.0.1 --port 8080
```

The `cat` example is intentionally a local-development configuration. For a
long-running service, use absolute paths owned by a dedicated service account
and a systemd unit generated or installed by the release installer. Do not run
the service as root and do not bind a remote listener until HTTPS is ready.

### Linux system service expectations

If you install the service manually, verify all of the following before
enabling it:

- `User` and `Group` identify a dedicated unprivileged account;
- the working directory contains the matching `redlib.toml`;
- the profile database and archive directory are writable only by that account;
- the service binds to loopback unless a private ingress rule is in place;
- the service has no password or token in its unit file; and
- the service restarts only after a failure, not in a tight loop.

The checked-in installer uses `vale.service`; verify the name it prints before
using service commands from a differently packaged release.

## Windows

The supported Windows paths use Docker Desktop Linux containers or WSL2. Vale
does not currently ship a native Windows service or `.exe` release contract.

### Recommended: WSL2

Install WSL2 from an elevated PowerShell prompt, restart if Windows requests
it, and open the installed Linux distribution:

```powershell
wsl --install
```

Inside WSL2, follow the [Linux instructions](#linux). A loopback Vale server
is normally reachable from the Windows browser at
`http://127.0.0.1:8080`. Keep WSL2 networking private; do not add a broad
Windows Firewall rule merely to make a remote device reach the first-run page.

The Linux installer needs systemd. Check it inside the distribution before
installing Vale:

```sh
systemctl status
```

Current Ubuntu distributions installed by `wsl --install` normally enable
systemd already. If another distribution reports that systemd is not running,
update WSL, enable `systemd=true` in that distribution's `/etc/wsl.conf`, run
`wsl.exe --shutdown` from PowerShell, and reopen the distribution. Follow
Microsoft's [WSL systemd guide](https://learn.microsoft.com/windows/wsl/systemd)
so an existing `wsl.conf` is edited rather than overwritten. Docker Desktop is
the simpler alternative when systemd cannot be enabled.

### Recommended: Docker Desktop

The repository's Windows installer uses Docker Desktop's Linux container
engine and the checked-in Compose file. First install
[Git for Windows](https://git-scm.com/install/windows) and a supported
[Docker Desktop for Windows](https://docs.docker.com/desktop/setup/install/windows-install/),
start Docker Desktop, and select its Linux-container engine. In PowerShell,
verify that all three commands work before cloning Vale:

```powershell
git --version
docker version
docker compose version
```

Then, from a PowerShell checkout:

```powershell
git clone https://github.com/Dankpaws/vale.git
Set-Location vale
git checkout v0.37.0
Set-ExecutionPolicy -Scope Process Bypass
.\install.ps1
```

It keeps the host port on loopback, builds the local image, waits for
`/healthz`, and prints the first-run URL. Open that URL and continue with
[First-run setup](#first-run-setup). The script never asks for an owner
password. Pass `-Port 9080` (or another free port) when 8080 is already in use;
the installer persists that choice in `.env`. The first image build compiles
the locked Rust graph and may take several minutes; keep Docker Desktop running
until the script reports a healthy container.

## Docker Compose

Docker is a good fit when Docker Engine or Docker Desktop is already managed
on the host. Install the supported
[Docker Engine and Compose plugin](https://docs.docker.com/engine/install/)
for your distribution first, and verify `docker version` and
`docker compose version` without changing the daemon's network exposure. Run
these commands from the repository checkout so the Compose file and
`.env.example` are from the same release:

```sh
git clone https://github.com/Dankpaws/vale.git
cd vale
git checkout v0.37.0
cp .env.example .env
```

Edit `.env` and set the Vale account and durable-state values. The exact
container paths must match the volumes declared in the checked-in Compose
file; the following names describe the required contract:

```dotenv
VALE_PORT=8080
VALE_PROFILE_MODE=accounts
VALE_PROFILE_DATABASE=/var/lib/vale/profiles.sqlite3
VALE_ARCHIVE_DIR=/var/lib/vale/archives
VALE_MEDIA_CACHE_DIR=/var/cache/vale
VALE_COOKIE_SECURE=off
REDLIB_FULL_URL=http://127.0.0.1:8080
REDLIB_DEFAULT_THEME=dark
REDLIB_DEFAULT_REMOVE_DEFAULT_FEEDS=on
REDLIB_ROBOTS_DISABLE_INDEXING=on
```

`VALE_MEDIA_CACHE_DIR` is the cache root; Vale creates and uses its
`video-downloads` child below that root. Keep this path on the separate cache
volume rather than placing it inside the profile volume.

Before starting, inspect the rendered configuration:

```sh
docker compose config
```

Confirm that:

- the image is the Vale image for this release, not an upstream Redlib image;
- the profile database and archive directory are mounted on durable volumes;
- the media cache is separate and disposable;
- the application runs as a non-root user;
- the host port is loopback-only for a local install; and
- no secret appears in the rendered configuration or command output.

Start and verify the service:

```sh
docker compose up -d
docker compose ps
curl --fail http://127.0.0.1:8080/healthz
```

Follow logs without using shell history or screenshots to record credentials:

```sh
docker compose logs -f vale
```

### Building the local image

If no release image is available, build the image from the checked-out source
instead of pulling an unrelated public image:

```sh
docker build -t vale:local .
```

The Dockerfile must support the architecture you selected and the resulting
Compose configuration must still provide durable writable volumes. Video
download support additionally requires FFmpeg in the image or an explicit
documented limitation; a missing optional binary must not make the first-run
flow fail.

### Do not delete state by accident

`docker compose down` stops the service but should leave named volumes intact.
Do not use `docker compose down --volumes` unless you deliberately intend to
delete profiles and saved archives. Make a backup first; see
[Backups and recovery](OPERATIONS.md#backups-and-recovery).

## Reverse proxy and HTTPS

Vale serves HTTP and expects a reverse proxy or another TLS terminator when
used remotely. Keep Vale on loopback when the proxy is on the same host. When
the proxy is on a different private host, bind Vale only to its private app
address and allow the app port from that proxy alone:

```text
browser --HTTPS--> private reverse proxy --HTTP/private firewall--> Vale:8080
```

Set the canonical origin to the exact external URL, including the scheme but
not a path:

```dotenv
REDLIB_FULL_URL=https://vale.example.test
VALE_COOKIE_SECURE=on
```

For the first Linux installation, those remote-mode values are explicit and
non-interactive:

```sh
sudo ./install.sh \
  --bind-address <private-app-address> \
  --cookie-secure on \
  --public-url https://vale.example.test
```

The listener and origin options are first-install choices. On an upgrade, the
installer preserves `/etc/vale/vale.env` and rejects those options instead of
silently ignoring or overwriting the protected configuration. To reconfigure
an existing service, back up and deliberately edit that file, restart
`vale.service`, and repeat the health, HTTPS-origin, cookie, and firewall
checks below before admitting traffic.

The proxy must preserve `Host`, set `X-Forwarded-Proto` to the client scheme,
and enforce its own private-network allowlist. Do not put a password in the
proxy configuration when Vale native accounts are enabled. Test the HTTPS
origin from a client on the intended private network before sharing it.

## First-run setup

After the process is healthy, open the URL printed by the installer. For a
local development process this is usually:

```text
http://127.0.0.1:8080/
```

For an HTTPS deployment, use the reverse-proxy URL instead. A new account-mode
database redirects dynamic pages to `/setup`.

1. Enter a display name (optional), a 3–32 character username, and a password
   between 12 and 128 characters. Confirm the password.
2. Review the browser-profile preview. Leave **Use this browser's current
   setup** checked only when importing that browser's existing feed,
   subscription, filter, and display preferences is intentional.
3. Select **Create owner profile**. Setup is a one-time operation; it closes
   after the first account is committed.
4. Open **Feeds**, create a named topic feed, and add communities. A community
   assigned to another feed moves instead of being duplicated.
5. Open **My feed**, choose Hot/New/Top/Rising/Controversial, and open one
   local post. The title should stay on Vale while the discussion loads.
6. Sign out and sign back in once. This verifies that the account database and
   secure session flow work before you add more devices.

If setup does not appear, check that `VALE_PROFILE_MODE=accounts` is present
and that the configured database path is writable by the service user. If
login appears to succeed but immediately returns to login on local HTTP,
check that the development-only `VALE_COOKIE_SECURE=off` setting is active.
Use HTTPS and turn it back on for any remote deployment.

## After installation: a complete smoke test

Run this checklist before considering the installation complete:

- [ ] `GET /healthz` returns `ok` from the intended listener.
- [ ] The browser reaches `/setup` only while the account database is empty.
- [ ] The owner account is created without a password in logs or config.
- [ ] The owner can create a named feed and fetch one community.
- [ ] A post opens locally, comments render, and a media card expands in place.
- [ ] **Saved** can queue an archive, and the archive status explains complete,
      partial, capturing, or failed results.
- [ ] Sign-out and sign-in work; a second account cannot see the first account's
      profile, history, hidden posts, or archives.
- [ ] Restarting the service or recreating the container preserves the profile
      and archives.
- [ ] Remote access, if enabled, uses HTTPS, private ingress, and secure
      cookies; the application port is not publicly exposed.
- [ ] You know where to find logs and how to restore a backup. See
      [Operations](OPERATIONS.md).

If Reddit or its OAuth compatibility endpoint is unavailable, `/healthz`,
setup, login, account isolation, and restart checks should still work. Record
feed, post, media, comment, and archive retrieval as externally blocked and
repeat those checks when Reddit access recovers; do not misreport local health
as failed.

## Next steps

- [Using Vale](USAGE.md) explains feeds, discussions, Search, media, Saved,
  profiles, keyboard shortcuts, and PWA installation.
- [Configuration and operations](OPERATIONS.md) explains paths, quotas,
  backups, updates, uninstalling, and troubleshooting.
- [Security](../SECURITY.md) explains the private-network and data-handling
  boundaries.
