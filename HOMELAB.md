# Vale fork notes

This checkout contains the Vale product fork of the Redlib Reddit reader. It
is a source and product contract, not a record of any particular private
deployment. Public installation and operator guidance lives in
[`README.md`](README.md), [`docs/INSTALL.md`](docs/INSTALL.md),
[`docs/USAGE.md`](docs/USAGE.md), and [`docs/OPERATIONS.md`](docs/OPERATIONS.md).

## Identity and upstream obligations

- Vale owns the active product name, interface copy, PWA metadata, account
  cookie, profile/archive formats, and user-facing documentation.
- The executable remains named `redlib` for upstream compatibility. The
  interface identifies Vale and credits the Redlib engine.
- Redlib's source, [AGPL-3.0 license](LICENSE), [CREDITS](CREDITS), and bundled
  third-party notices remain available when this fork is redistributed.
- Public examples must use neutral hostnames and synthetic accounts. Never
  commit private deployment names, internal addresses, credentials, session
  values, private keys, or operator-only recovery material.

## Product contract

Vale is a quiet, subscription-first reader for selected communities. It is
not a general Reddit clone and does not require a Reddit account.

- Named feeds remain separate. A community belongs to one named feed at a time;
  All and Popular are not normal destinations.
- `/f/<feed>/<sort>` is the canonical listing shape. Supported sorts are Hot,
  New, Top, Rising, and Controversial. Empty or unknown feeds must not silently
  fall back to another feed.
- `/feeds` manages up to eight named feeds and 32 assigned communities. The
  active feed is device-local; the feed library and profile preferences sync
  for the signed-in account.
- The Vale brand is the canonical feed-home link. Primary navigation contains
  Feeds, Saved, Search, and Account; feed selection stays in the page and theme
  selection stays in Settings instead of becoming duplicate header controls.
- Titles open local Vale post pages. Submitted text, images, galleries, GIFs,
  and videos expand in place through one footer control. Thumbnails are inert
  previews. Ordinary video is audio-validated HLS only; GIF animation may use
  Reddit's MP4 derivative, and explicit video download requires both video and
  audio in one bounded copy-remux. Explicit source links are the deliberate way
  to leave Vale.
- Discussions use one normalized flat projection with stable comment identity,
  parent context, depth, continuation state, and honest incomplete coverage.
  Loading a continuation does not change the post URL.
- Exact canonical URLs and exact Reddit crosspost identities may be grouped
  into a combined discussion. Similar titles or cross-domain stories never
  cause an automatic merge.
- Search defaults to the active named feed; searching all Reddit is explicit.
  Results identify their feed membership and comment search preserves thread
  context. The retired `/discover` surface only issues a private, no-store
  redirect to canonical community search and never performs its own retrieval.
- Community pages keep one same-origin icon/fallback and an inert, sanitized
  information rail. At 1120 CSS pixels and below that rail precedes the post
  list; wide layouts keep it beside the list.
- Eligible post listings expose at most 25 top-level representatives. A request
  may consume four sequential 25-record upstream pages, applying profile,
  hidden, NSFW, identity, and exact named-feed grouping rules before committing
  one deterministic snapshot. Versioned `posts-v1` fragments are private,
  bounded, shell-free responses; ordinary links and native Hide forms remain
  the complete no-JavaScript path.
- Hide is an immediate profile-scoped action with a 12-entry Undo bound and
  keyed in-place replenishment. Cross-tab, failure, uncertain-write, and
  BFCache recovery use authoritative hidden-state verification before a fresh
  snapshot may unhide or replace a card. History is separate from hidden state
  and does not imply unread semantics.
- Saved creates a profile-owned standalone archive with HTML, Reddit JSON,
  comments, captured assets, a manifest, byte counts, and SHA-256 checksums.
  Complete, partial, capturing, cleanup, deleting, and failed states remain
  distinct. Reader v3 is script-free, self-contained, dark by default, light
  and print aware, normalizes archived comment headings beneath the reader
  outline, and records bundled reader-support files separately from
  captured/source assets. Version-1 and Reader-v2 manifests remain readable
  and immutable; regeneration of a manifest newer than the supported reader
  version fails closed.
- Native accounts are the supported private-instance model. There is no public
  registration. The first account is created once at `/setup`; administrators
  create independent accounts and can reset passwords or disable accounts.
- The responsive desktop and mobile interface is one information architecture.
  PWA shell assets are immutable and local; profile pages, feeds, comments, and
  media are not service-worker offline data.

## Runtime contract

- The server listens on a configured HTTP address and port. TLS belongs at a
  trusted reverse proxy or another explicitly configured TLS terminator.
- A safe install starts on loopback. Remote access requires HTTPS, private
  ingress, a matching canonical origin, and secure cookies.
- `VALE_PROFILE_MODE=accounts` enables the native profile database. The first
  account-mode start redirects dynamic pages to `/setup` until the owner is
  created.
- `VALE_PROFILE_DATABASE` stores accounts, profiles, sessions, feeds,
  preferences, history, hidden posts, and archive records. `VALE_ARCHIVE_DIR`
  stores durable snapshots. `VALE_MEDIA_CACHE_DIR` is disposable and must stay
  separate from profile state.
- Passwords are salted Argon2id hashes. Session tokens are random values whose
  SHA-256 hashes are stored. Secure deployments use `Secure`, `HttpOnly`,
  `SameSite=Lax`, `__Host-` session cookies.
- Dynamic/profile responses are `private, no-store`. Authenticated proxied
  media may use private HTTP caching; the service worker still caches only
  immutable shell assets.
- Archive capture uses one transactional profile/instance quota snapshot and a
  durable fixed reservation before work is queued. Profile budgets are either
  the instance maximum or whole 256 MiB steps; lowering a limit never revokes
  an admitted job or deletes an existing archive. Startup reconciliation,
  final publish, cleanup, and deletion retain exact accounting until durable
  filesystem and database state agree. Capture is also bounded by record,
  comment, continuation, file, redirect, and processing-time limits. Captured
  external HTML is sanitized and inert.
- Vale does not add an external authentication gateway, public registration,
  Reddit credentials, advertising, analytics, tracking, or a public listener
  as part of its reader contract.

## Configuration boundaries

`redlib.toml` and environment variables are the configuration sources;
environment variables take precedence. `REDLIB_*` names retain upstream engine
compatibility. `VALE_*` names configure accounts, sessions, storage, and
archives. New public documentation must not expose a real deployment's origin,
network range, state location, certificate path, or secret-management path.

The release should provide a safe account-mode baseline equivalent to:

```toml
VALE_PROFILE_MODE = "accounts"
VALE_PROFILE_DATABASE = "/var/lib/vale/profiles.sqlite3"
VALE_ARCHIVE_DIR = "/var/lib/vale/archives"
VALE_SESSION_DAYS = "30"
VALE_COOKIE_SECURE = "on"
REDLIB_DEFAULT_REMOVE_DEFAULT_FEEDS = "on"
REDLIB_ROBOTS_DISABLE_INDEXING = "on"
```

Loopback HTTP development may set `VALE_COOKIE_SECURE = "off"`; that value
must never be used as a shortcut for remote deployment.

## Deliberate feature boundaries

These are not dormant promises or reasons to add decorative controls:

- Community and author filtering has bounded existing support; broader domain,
  flair, and phrase muting requires explicit matching and persistence rules.
- Submitted text uses explicit **Read post** disclosure; automatic previews
  require a separate product decision.
- Related-coverage discovery must stay separate from exact-identity grouping and
  must provide reversible user-controlled merge/split behavior.
- History and hidden posts do not stand in for read/unread or “since last
  visit” state.
- A digest requires a scheduler, ranking budget, and reset semantics before it
  can be exposed.

## Build and verification

Use Rust 1.88 or newer (current stable is recommended) with the locked
dependency graph:

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo build --release --locked --bin redlib
node --test tests/js/*.test.mjs
```

Live Reddit/OAuth assertions are ignored in the deterministic default suite.
Run them separately with
`cargo test --locked -- --ignored --test-threads=1`, and record external
OAuth, rate-limit, DNS, and changing-Reddit-response failures separately from
local regressions.

Every release should also verify, with synthetic data:

- first-run `/setup`, owner creation, profile import wording, and setup closure;
- two-account isolation and expected session revocation;
- named-feed separation, canonical routes, and device-local active feed;
- local post, inline media/text, comment continuation, search, hide, history,
  and archive behavior;
- 320–1440 CSS-pixel layouts, keyboard and touch access, no page overflow, and
  no shadow-based elevation;
- private cache headers, service-worker scope, HTTPS/cookie behavior, and
  non-root service/container execution; and
- backup, restart, upgrade, rollback, and uninstall behavior without deleting
  profile state accidentally.

## Public documentation boundary

The public guide must be usable by a stranger who knows nothing about Vale. It
must include prerequisites, one copy-paste path for each supported platform,
the first-run checklist, feature explanations, troubleshooting, safety
warnings, backup/recovery, update/uninstall guidance, and accessible visual
diagrams or sanitized screenshots. Every command must be repository-relative or
name the canonical `Dankpaws/vale` repository and an exact release tag.
