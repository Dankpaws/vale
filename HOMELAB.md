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
- The Vale brand and Feed navigation resolve `/` to the active canonical named-feed URL, so Hide and Undo refresh through a fragment-eligible route. Primary navigation contains
  Feed, Reading, Saved, Search, and Account; feed management remains at `/feeds`; feed selection stays in the page and theme
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
  list; wide layouts keep it beside the list, sticky beneath the header at the
  same offset as feed context. The desktop rail is bounded by the viewport;
  long expanded content scrolls within the keyboard-focusable rail.
- Eligible post listings expose at most 25 top-level representatives from a
  100-record raw budget. Grouped named-feed listings request the remaining
  budget in one upstream batch; ordinary ungrouped listings keep a cheap
  25-record first request and fetch the remaining budget only when filtering
  leaves vacancies. If Reddit under-delivers a requested batch, the safe
  cursor fallback may use up to four requests. Profile, hidden, NSFW, identity,
  and exact named-feed grouping rules are applied before committing one
  deterministic snapshot. An underfilled snapshot retains its safe
  continuation cursor, and enhanced replenishment follows empty windows until
  visible posts or Reddit's actual end. Versioned `posts-v1` fragments remain
  private, bounded, shell-free responses; ordinary links and native Hide forms
  remain the complete no-JavaScript path.
- Hide is an immediate profile-scoped action with a 12-entry Undo bound and
  keyed in-place replenishment. Cross-tab, failure, uncertain-write, and
  BFCache recovery use authoritative hidden-state verification before a fresh
  snapshot may unhide or replace a card. History is separate from hidden state
  and does not imply unread semantics.
- Comment activity is profile-owned, per Reddit post, and separate from unread
  state. Opening a discussion establishes a comment-total/time baseline; first
  visits have no delta. Listings show positive net growth only, using saturating
  subtraction, never a negative value. A lower total becomes the next baseline.
  Sorting, searching, clearing search, comment links, and continuation patches
  carry a profile/post-bound visit ID and retain that visit's comparison even
  when another tab opens the post. Browser reload requests establish a new
  baseline. Combined discussions keep independent source-post comparisons.
  Comments created after the prior visit receive a 4% teal wash and a small
  accessible New label, including subsequently loaded replies. Counts and
  highlights are different signals: net growth may be zero despite new replies.
  No polling or new Reddit requests are added to listings. Upstream cache lag
  and incomplete comment coverage still apply. Activity starts on the first
  post visit after upgrade; historical visits are not backfilled.
  Baselines retain up to 5,000 posts/180 days per profile. Stable visit snapshots
  retain up to 512 visits/24 hours; expired or foreign IDs cannot advance state.
  Clearing history also clears activity. Browser-only profile mode has no
  server-owned activity tracking.
- Make a permanent copy creates a profile-owned standalone archive with HTML, Reddit JSON,
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
- `/` renders the selected named feed directly. Profiles without a named feed
  redirect to `/feeds`; pagination and fragment loading use canonical sorted
  `/f/<feed>/<sort>` URLs.

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
- Account-mode startup selects and verifies SQLite WAL mode and runs schema
  initialization before serving. Individual connections retain their busy
  timeout, foreign-key enforcement, and full synchronous durability. The
  process-liveness health route performs no database work, and a single-post
  hidden check uses the profile/post primary key rather than materializing the
  profile's complete hidden set.
- Passwords are salted Argon2id hashes. Session tokens are random values whose
  SHA-256 hashes are stored. Secure deployments use `Secure`, `HttpOnly`,
  `SameSite=Lax`, `__Host-` session cookies.
- Dynamic/profile responses are `private, no-store`. Authenticated proxied
  media may use private HTTP caching; the service worker still caches only
  immutable shell assets. Shell hits return immediately without starting a
  background network refresh; misses fetch once and populate the shell cache.
- Archive capture uses one transactional profile/instance quota snapshot and a
  durable fixed reservation before work is queued. Profile budgets are either
  the instance maximum or whole 256 MiB steps; lowering a limit never revokes
  an admitted job or deletes an existing archive. Startup reconciliation,
  final publish, cleanup, and deletion retain exact accounting until durable
  filesystem and database state agree. Capture is also bounded by record,
  comment, continuation, file, redirect, and processing-time limits. Captured
  external HTML is sanitized and inert. Explicit capture jobs run FIFO on a
  dedicated single-thread runtime with one blocking worker; on Linux, both are
  scheduled at niceness 10 so page-serving work retains interactive priority.
- Existing reading interactions avoid unrelated work: comment controls update
  their owning thread group, navigation anchors measure each candidate once,
  and unchanged scroll frames do not rewrite jump controls or serialize Settings.
  Combined discussions retrieve at most three sources concurrently, preserve
  their requested order, and validate all identities before recording visits.
- Topic observations use one durable transaction, with no writer acquisition
  for an empty eligible-topic set. Unfollowed discussions skip watch capture;
  observers recheck follow state after waiting for a writer. Timed-out or
  cancelled media remuxes terminate their FFmpeg child process.
- Cancelled Reddit token refreshes release their in-progress flag, allowing
  later refresh attempts even when another grouped source fails.
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

- Community and author filters complement explicit domain, flair, title-phrase,
  post-type, and episode rules. Keep matching and persistence bounded.
- Submitted text uses explicit **Preview text** disclosure; automatic previews
  require a separate product decision.
- Related-coverage discovery must stay separate from exact-identity grouping and
  must provide reversible user-controlled merge/split behavior.
- History and hidden posts do not stand in for read/unread state; comment
  activity uses its own explicit visit baselines.
- Editions use explicit size caps, one-feed membership, revisioned progress, and
  opt-in schedules. They do not imply exhaustive coverage or automatic archives.

## Reading flow contract (2026-09-04)

The current source refines feed → inline preview → local discussion → Back.

- Feed titles are complete, use the local post route, and carry the primary
  hierarchy. Scores sit beside the timestamp. Preview, discussion, and Hide actions form
  one reading cluster, with a quiet separator before Hide. Text cards do not
  reserve an empty thumbnail. The desktop feed column caps at 1200px so text and
  media cards share an edge; the discussion layout caps at 1180px with a 240px
  reading-tools sidebar. Both compositions are centered with automatic inline
  margins; their rem-based caps yield to available width under browser zoom.
  At 1120 CSS pixels the sidebar gives way to the reading column and the
  discussion tools reflow above it.
- Collapsed cards share a 140px minimum height and grow for actual text. Desktop
  thumbnails use a 176×112px proportional image beside metadata, title, and
  actions; mobile uses 88×88px thumbnails with a full-width action row. Images
  use contain fitting against the quiet surface rather than cropping. Media
  thumbnails are keyboard-accessible inline-preview buttons where an inline
  panel exists; other thumbnails link to the local post. History focus preserves
  the thumbnail control as well as the existing reading controls.
- Inline previews have synchronized opening and closing disclosures, with a
  Close preview and Read comments pair at the end. Closing from inside the panel
  returns focus to its opener without leaving it above the viewport.
- Discussion prose uses a 72ch measure. Comment paragraphs share that measure inside the wide layout. Comment bodies
  retain the flat thread projection and capped depth indentation. Scores are plain metadata; separate
  chevrons collapse comments. Reply labels stay concise, with the author retained
  in the accessible name. The default sort reads Best and still submits confidence.
- Back restores expanded previews, exact-source disclosures, the reading anchor,
  and the activated title/preview/discussion control. Font readiness is bounded
  before restoring the anchor. Pointer activation does not scroll the card first.
- Secondary pages share the listing/reader width tokens, compact heading scale,
  solid surfaces, and 44px controls. Author and search comment excerpts use
  native details disclosures; other-discussion results reuse feed cards. Wiki,
  Saved, History, settings, authentication, and notice pages retain their own
  function while using the same typography and spacing. Existing standalone
  archive snapshots remain self-contained and unchanged.
- The mobile reading toolbar fits all three actions at 320px. Existing local
  interactions, named-feed isolation, bounded listings, and account controls remain
  the foundation of the flow.

Real-template synthetic fixtures and their loopback review server are documented
in `tests/reading/README.md`. They complement the runtime and signed-in candidate
checks; they do not emulate persistence, account operations, or Reddit retrieval.

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

Archive retrieval and async filesystem operations may remain on the isolated
archive runtime, but large checksums, HTML/source transforms, manifest/reader
rendering, and gallery ZIP construction must use that runtime's one-thread
blocking pool. Keep the existing archive and gallery concurrency/memory limits
unless a separately reviewed streaming design replaces them.

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

## Browser efficiency contract (2026-09-04)

Closing or hiding a media preview tears down its player, pending preparation,
network requests, and media source. Reopening prepares it again and restores the
saved playback time after metadata is available; autoplay preferences still apply.
Scroll alone does not serialize comment patches into browser history. Explicit
presentation changes, navigation, pagehide, and backgrounding preserve reading
state. Navigation captures the current anchor before leaving.

The UI uses solid surfaces without decorative backdrop filters. Spoiler/NSFW
content blurring retains its functional purpose. The service worker precaches the
same versioned CSS/player/interaction URLs emitted by the page, with the package
version injected by the server. Its revision cache must be bumped with shell
changes; page URLs and precache URLs must stay in sync (covered by the JS suite).
Old or unrecognized asset revisions pass through to normal HTTP handling.

## Public documentation boundary

The public guide must be usable by a stranger who knows nothing about Vale. It
must include prerequisites, one copy-paste path for each supported platform,
the first-run checklist, feature explanations, troubleshooting, safety
warnings, backup/recovery, update/uninstall guidance, and accessible visual
diagrams or sanitized screenshots. Every command must be repository-relative or
name the canonical `Dankpaws/vale` repository and an exact release tag.

## Reading workspace

Reading adds explicit discussion checkpoints and follows, stable feed windows,
finite editions, scoped filters and episode boundaries, saved-comment notes and
collections, search and exports, encrypted device offline packs with revisioned
replay, manually curated stories, selected RSS/Atom sources, topic watches, and
opt-in schedules. See [`docs/USAGE.md`](docs/USAGE.md) for behavior and limits.

All state remains profile-owned. CSRF/origin checks and private response caching
apply to new routes. Only the empty offline shell and static assets are cached
by the service worker; authenticated navigation uses the network and falls back
to the locked reader when disconnected. Native form compatibility uses
`Referrer-Policy: same-origin`, retaining opaque-origin CSRF rejection.

Full-discussion checkpoints use bounded context recovery. Discussion controls
share the sticky rail; compact screens retain Previous/Next with other actions
in Reading options. Repeated navigation preserves button focus. The rail groups
navigation, Your place, Discussion, and Go to with flat left-aligned actions.

AI summarization and LLM integration are deferred; no AI endpoint is included.

## Comment reading refinements

Comment-count and preview-discussion links open the post at its top. Explicit
comment permalinks and Resume links retain their targets. Back, reload, and
restored history entries retain the existing browser reading-state recovery;
fresh feed opens do not automatically resume an older checkpoint.

A quiet outlined star saves a comment without navigating away. Successful saves
fill the star and announce confirmation; clicking again unsaves it. Saved IDs
and revisions are loaded once per rendered post, including continuation
fragments, so reloads and explicit comment pages retain the correct action.
Failures retain a retryable control. Native form fallback returns to the comment. The persistent
checkpoint is one bookmark icon in the sticky desktop rail (or the compact
mobile toolbar), with an accessible name and concise explanation in Reading
options. Checkpoints still use revision checks and bounded context recovery.

GIPHY GIF page, embed, and supported direct-media links inside comments render
as lazy inline images with a GIPHY source link. Only validated alphanumeric IDs
reach the fixed same-origin `/reaction/giphy/:id` proxy; media remains authenticated
and privately cached. No third-party iframe, script, API key, or browser request
to GIPHY is introduced. Other reaction sites and unrecognized links remain links.

The Reading sidebar preference chooses Left (default) or Right on desktop and
persists through the normal profile/cookie and versioned export paths. Mobile
keeps its compact toolbar. Comment stars occupy a fixed trailing metadata slot;
the glyph is one text em, with a larger invisible pointer target. Unsave retains
profile isolation and revision checks from the existing library removal path.

## Contextual search picker

Focusing either the desktop header search or the full search-page query reveals
an in-form scope picker. The checked default searches the current named feed,
with a selector when multiple feeds exist, or the current r/community on
community/discussion routes. Unchecking searches all Reddit. Scope changes are
local to the search form and use the existing server routes and feed membership;
they do not edit feed membership or profile settings. Escape, leaving the form,
and clicking outside dismiss the picker. The mobile search link carries the
current community into the full search page. Existing native forms remain the
fallback when JavaScript is unavailable.

## Stable inline-preview layout

Inline panels occupy an explicit grid row below listing titles and actions.
Desktop expansion hides the thumbnail visually while retaining its grid space,
so the title's measure and metadata/action positions remain fixed. At phone
widths, reading actions precede the thumbnail; expansion replaces the thumbnail
below those controls. Toggle labels reserve their collapse-label width so the
comment count and Hide action do not shift sideways. Only the full preview image
is visible while expanded.

## Unified Saved workspace

Saved presents bookmarked posts, retained comments/notes/collections, and
archives through All / For later / Comments / Copies. Post identity deduplicates
bookmarks, annotations, and archives; individual comments remain separate.
Save/Saved toggles the existing bookmark flag in place. Mark finished uses one
additive default-false `reading_entries.finished` column, retaining saved
membership; moving back to For later or re-saving clears it. Reads never finish
items. Unsave does not delete copies or written notes. Existing comment-star
removal and explicit archive deletion remain unchanged.

Reading now contains explicit places and follows. Clearing a place preserves
saved membership and follows. Legacy `/reading?list=later` and library GET links
redirect to Saved; existing mutation routes and stored evidence stay compatible.
Post annotations reuse the existing empty-comment library identity and its
revisioned editor. Merely opening that editor does not create a visible retained
item after unsaving. Saved selection is profile-scoped and limited in SQL to
50 rows plus a continuation probe before loading excerpt bodies.

Save options offers explicit permanent copying. Server copies and device offline
packs remain separate, with existing budgets, private caching, CSRF checks, and
failure/partial states. No automatic capture, finish, or device download occurs.


## Reading overview

Reading opens a feed-scoped overview: one explicit place kept within seven days,
up to six followed-discussion updates ordered by oldest visible unread observation,
and optionally an existing recent unfinished edition. Continue retains old places;
Following retains unassigned follows. Source/topic tools stay separate. Quiet,
partial, failed, pending/stale and snoozed checks have distinct wording.

Catch-up uses the exact displayed reply identities, profile/revision and
preference/branch scope. Later arrivals and undisplayed replies stay unread;
acknowledgement leaves the kept place, saved membership and follows unchanged.
The old generic caught-up command is rejected. Additive default-zero fields track
explicit place time, successful capture time and per-comment acknowledgement;
legacy timestamps are not backfilled as proof of recent intent/success.
See [the Reading guide](docs/USAGE.md#reading) for user-facing behavior.


Checkpoint capture excludes top-level continuation placeholders from
checkpoint disclosure state, preserving actual comment expansion/collapse state
without relaxing server identity validation. A focused JavaScript regression covers this case.
