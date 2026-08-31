# Vale

Vale is a quiet, self-hosted Reddit reader for the communities you choose. It
keeps ordinary reading on the Vale origin, stores profile preferences on the
Vale server, and proxies Reddit media through the same origin. Vale is built
from a maintained [Redlib](https://github.com/redlib-org/redlib) fork; Redlib
remains the retrieval and rendering engine and its AGPL-3.0 notices and credits
remain in this repository.

Vale is not a Reddit account client. It does not ask for a Reddit username,
password, or developer credential. Reddit retrieval uses an unofficial,
installed-client compatibility flow and can be rate-limited or changed by
Reddit without notice. Read [Privacy and security](SECURITY.md) before making
an instance reachable by anyone else.

> [!WARNING]
> A Vale server contains account profiles, reading history, hidden posts, and
> any offline archives that users save. Start with a loopback listener. If you
> need access from another device, put Vale behind HTTPS and a private network
> boundary before binding it to a non-loopback address. Never expose a fresh
> HTTP listener directly to the public internet.

## Choose an installation path

The detailed, copy-paste instructions are in [Installation](docs/INSTALL.md).

| Path | Best for | What it provides |
| --- | --- | --- |
| Linux service | A small private server or workstation | A pinned binary or source build, durable state, and a hardened systemd service |
| Windows + Docker Desktop | The simplest supported Windows path | A PowerShell installer for the Linux-container Compose stack |
| Windows + WSL2 | Windows users who prefer a Linux distribution | The Linux source/service path inside a private WSL2 environment |
| Docker Compose | Docker Engine on Linux or Docker Desktop | A reproducible container with explicit profile/archive/cache volumes |

The checked-in Compose configuration is the source of truth for container
defaults. Do not substitute an upstream Redlib image for a Vale image: it will
not contain Vale's account and archive behavior.

## Quick start

For a local Linux checkout with the documented build prerequisites installed,
the checked-in installer builds Vale, creates its unprivileged service account,
installs the systemd unit, and waits for the health check:

```sh
git clone https://github.com/Dankpaws/vale.git
cd vale
git checkout v0.36.1
sudo ./install.sh
```

Use an exact tagged release-asset URL to avoid a local Rust build; the complete
commands and prerequisite split are in [Linux installation and first
run](docs/INSTALL.md#linux). The Cargo binary remains named `redlib` for
upstream compatibility; installed artifacts and the reader interface use Vale.

For Docker, use the repository's Compose file after creating a local `.env`
from the documented example:

```sh
cp .env.example .env
docker compose config
docker compose up -d
```

The Compose service must mount durable locations for the profile database and
archives. Verify that with `docker compose config` before the first login, then
open [First run](docs/INSTALL.md#first-run-setup).

## First run in three minutes

1. Open the URL printed by the installer, normally `http://127.0.0.1:8080` for
   a loopback development install or the HTTPS URL configured by your reverse
   proxy.
2. With an empty account database, Vale redirects dynamic pages to `/setup`.
3. Create the owner username and a password of at least 12 characters. The
   password is stored as an Argon2id hash; it is not written to the config
   file.
4. Leave **Use this browser's current setup** checked only if you want this
   browser's existing Vale-compatible feed, subscription, filter, and display
   preferences imported. The active feed remains device-local.
5. After Vale signs you in, open **Feeds**, create a named feed, and add the
   communities for one topic. Open **My feed** and choose a sort.
6. Read [Using Vale](docs/USAGE.md) for inline media, discussions, Search,
   Saved, keyboard navigation, PWA installation, and account settings.

Setup closes after the first account is created. If another person needs
access, an administrator creates a separate account from **Settings**; there
is no public registration. If the only administrator cannot sign in, use the
local [password recovery command](docs/OPERATIONS.md#reset-an-account-password-from-the-local-binary)
instead of editing the database or putting a password in a command line.

## What Vale does

- **Named feeds:** Keep topics separate instead of mixing every subscription
  into one stream. A community belongs to one named feed at a time.
- **Local reading:** Open post titles, submitted text, images, galleries, GIFs,
  videos, and comments without leaving Vale. Explicit source links are still
  available when you choose them.
- **Discussions:** Load truncated comment branches in place. Child trees can
  be collapsed, keyword-filtered bodies can be revealed, and browser Back
  restores loaded branches and reading position.
- **Search:** Search the active named feed by default, or deliberately choose
  all Reddit results. Results show their feed membership.
- **Saved:** Save a profile-owned offline snapshot with a self-contained,
  script-free HTML reader, Reddit JSON, captured media, a manifest, byte counts,
  hashes, durable admission accounting, and an optional profile storage budget.
- **Profiles:** Synchronize feeds, subscriptions, filters, preferences,
  history, hidden posts, and archives across devices using one Vale account.
- **PWA:** Install Vale from a supported browser. The service worker caches
  only immutable shell assets; feeds, profiles, and media remain on the
  network/private HTTP cache.

See [Using Vale](docs/USAGE.md) for the complete feature guide and boundaries.

## Configuration at a glance

Vale reads environment variables and, when present, `redlib.toml` in its
working directory. Environment variables take precedence. A safe account-mode
baseline looks like this (replace paths and origin for the host):

```toml
VALE_PROFILE_MODE = "accounts"
VALE_PROFILE_DATABASE = "/var/lib/vale/profiles.sqlite3"
VALE_ARCHIVE_DIR = "/var/lib/vale/archives"
VALE_SESSION_DAYS = "30"
VALE_COOKIE_SECURE = "on"
REDLIB_FULL_URL = "https://vale.example.test"
REDLIB_DEFAULT_THEME = "dark"
REDLIB_DEFAULT_REMOVE_DEFAULT_FEEDS = "on"
REDLIB_ROBOTS_DISABLE_INDEXING = "on"
```

For loopback-only HTTP development, use a private bind address and set
`VALE_COOKIE_SECURE = "off"` for that development instance only. Re-enable it
when the canonical browser origin is HTTPS. See [Configuration and operations](docs/OPERATIONS.md)
for all Vale-specific paths, quotas, backups, updates, and recovery.

## Development

Source builds require Rust 1.88 or newer; current stable is recommended. The
project uses the locked Rust dependency graph. The normal checks are:

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo build --release --locked --bin redlib
```

The default suite is deterministic. Tests that make live Reddit/OAuth requests
are explicitly ignored; run them separately with
`cargo test --locked -- --ignored --test-threads=1` when validating external
compatibility, and classify those failures separately from local regressions.
Read [Contributing](CONTRIBUTING.md) before changing behavior or public
documentation.

## Attribution and license

Vale is distributed under the [GNU Affero General Public License v3.0](LICENSE).
The [CREDITS](CREDITS) file records upstream and bundled third-party work.
Vale-specific changes should retain the upstream notices and identify the
Redlib engine when describing the product.

## Documentation map

- [Installation](docs/INSTALL.md) — prerequisites, Linux, Windows, Docker,
  first run, reverse proxy, and verification.
- [Using Vale](docs/USAGE.md) — concepts, features, limits, and everyday
  workflows.
- [Configuration and operations](docs/OPERATIONS.md) — state, backups,
  recovery, updates, uninstall, logs, and troubleshooting.
- [Security](SECURITY.md) — threat model, private-network boundary, data
  handling, and responsible vulnerability reports.
- [Contributing](CONTRIBUTING.md) — local development, validation, and review
  expectations.
- [Publishing](docs/PUBLISHING.md) — clean-history requirements for the first
  public repository and release checks.
- [Changelog](CHANGELOG.md) — user-visible release changes and compatibility
  notes.
- [Third-party notices](THIRD_PARTY.md) — upstream Redlib, bundled assets, and
  redistribution obligations.
- [Fork notes](HOMELAB.md) — concise product contract for maintainers; it does
  not contain a deployment's private topology or credentials.
