# Changelog

This file records user-visible Vale changes. Release entries use calendar dates
and immutable Git tags; externally caused Reddit compatibility changes are
called out separately from local regressions.

## Unreleased

No changes recorded yet.

## 0.37.0 - 2026-09-04

Vale gains a dedicated Reading workspace: a place to return to discussions,
finish a finite edition, follow selected sources, and retain useful passages.
This release also collects the reading, performance, and interface improvements
made since the August 30 release.

### Added

- **Reading** joins **Feeds / Reading / Saved** in primary navigation, including
  compact layouts. Read later, discussion follows, and reading checkpoints have
  their own home, separate from saved offline archives.
- **Keep my place** records a position inside the full discussion, including
  comment sort, relative viewport position, and expanded branches. Continue
  reading restores that context; a bounded recovery request can locate a comment
  outside the initial batch. Missing or filtered targets receive an explanation.
- **Jump through** top-level branches, new comments, the original poster's
  comments, or search matches with Previous/Next. Desktop controls remain in the
  sticky sidebar; compact layouts keep navigation available while scrolling.
- **Followed discussions and replies** retain bounded observations of selected
  threads or branches. The first capture establishes a baseline; edits are not
  counted as new replies, and absence from a partial capture is not deletion.
- **Keep this page** saves a stable window of up to 25 rendered feed cards.
  **Editions** create finite, deduplicated selections from one named feed's recent
  posts, giving quieter communities space alongside busy ones. Progress,
  completion, and reopening persist. Optional six-hour or daily schedules can
  create editions automatically.
- **Reading filters** support feed-scoped or global domain, flair, title phrase,
  and post-type rules, hide/only modes, temporary snoozes, and explicit episode
  boundaries for recognized season/episode labels.
- **Saved comments** retain original and parent context, editable notes, and
  collections. Search saved text and notes, explicitly index saved archives, and
  export retained material as JSON.
- **Encrypted device offline packs** hold explicitly selected editions, comments,
  or archives, with optional media. The offline PWA opens a locked reader;
  passphrase-protected packs support encrypted export/import. Queued notes and
  reading progress synchronize explicitly, with conflict review instead of
  silently overwriting newer server state.
- **Sources** follow selected RSS/Atom feeds inside a named feed. Initial capture,
  manual and periodic refresh, publication ordering, unread state, and explicit
  mark-read actions make updates actionable. Exact-URL matches connect source
  entries to observed Reddit discussions in the same feed.
- **Stories** organize retained passages, dated evidence, personal assessments,
  and stages from watching through resolved. Add source entries directly, link
  related same-feed material, and undo associations. Source counts describe the
  retained evidence; they do not claim consensus.
- **Topic watches** track chosen title phrases in observed posts within a named
  feed, with snooze and acknowledgment controls.
- Profile-isolation, revision-conflict, input-boundary, offline, migration, and
  browser regression coverage, plus synthetic reading-layout fixtures.

### Changed

- Refined the feed → inline preview → discussion → Back loop: complete titles,
  quieter metadata, adjacent reading actions, preview-footer navigation, larger
  actionable thumbnails, and restoration of the exact reading control.
- Aligned community, author, search, related-discussion, wiki, Saved, History,
  settings, and authentication surfaces with the reading layout. Responsive
  widths, solid surfaces, touch-sized controls, and keyboard disclosures keep
  desktop and mobile behavior consistent.
- Organized the discussion sidebar around navigation, Your place, Discussion,
  and Go to, with consistent flat action rows and concise navigation feedback.
- Added profile-owned comment activity: positive net growth since the previous
  discussion visit and a subtle highlight for comments created since then.
  First visits establish a baseline; counts never display negative growth.
- Serve the selected feed directly at the home URL, avoiding an extra redirect.
- Batch grouped-community listing retrieval into a bounded upstream request.
- Move buffered response compression and CPU-heavy archive/gallery preparation
  off asynchronous request workers. Explicit archive jobs use a separate bounded
  worker; Linux lowers its scheduling priority. Brotli uses explicit quality 5.
- Initialize SQLite WAL at startup, use direct hidden-post lookups, and keep the
  process-liveness endpoint free of database work.
- Tear down closed media previews and restore playback position when reopened.
  Capture navigation state at meaningful events instead of serializing it on
  every scroll. Remove decorative backdrop filters from reading surfaces.

### Fixed

- Hidden-post replenishment now follows safe continuation cursors across empty
  listing windows instead of stopping after the first four upstream pages.
- Long community About content stays scrollable inside the sticky desktop rail,
  including short viewports and keyboard navigation.
- Cached immutable PWA assets no longer trigger redundant network refreshes.
  Precache URLs match the versioned assets actually requested by pages.
- Separate comment and reply disclosures preserve thread context and focus;
  narrow-screen reading controls reflow without horizontal overflow.
- Native form submissions work with a same-origin referrer policy while retaining
  CSRF checks and excluding referrers on cross-origin requests.
- Remove the initial “Navigation covers loaded comments” block from the reading
  toolbar; meaningful position feedback appears after navigation.

- Use Debian's official HTTPS security origin in the container builder to avoid
  false CDN 404 responses for encoded package-version URLs. Package signature
  and checksum verification remain enabled.

### Scope and upgrade notes

- Reading features use server-backed profiles. Back up the profile database and
  archives before upgrading; startup adds the new tables. Preserve a matching
  pre-upgrade backup if a downgrade is needed.
- An edition is a saved selection of post cards, not an automatic archive of its
  threads or media. Choose an offline pack explicitly for disconnected reading.
  The 5/10/20-minute edition choices are approximate size presets, not measured
  reading-time guarantees.
- Observations and navigation cover bounded, retrieved content. Topic watches
  are not exhaustive Reddit monitoring; source links use exact URLs rather than
  fuzzy matching. Sources can only retrieve permitted public HTTP(S) endpoints.
- AI summarization and LLM integration are intentionally deferred. Stories and
  assessments remain reader-curated.

## 0.36.1 - 2026-08-30

### Fixed

- Made the optional Debian and Ubuntu FFmpeg installation use explicit
  conditional control flow so the tagged ShellCheck gate passes while
  preserving the installer's best-effort fallback behavior.

## 0.36.0 - 2026-08-29

### Added

- Native, isolated Vale accounts with one-time owner setup, administrator-led
  account creation, session revocation, and an offline password-reset command.
- Separate named feeds, local reading interactions, bounded comment
  continuation, profile history/hidden state, and profile-owned offline
  archives.
- Installable, safe-area-aware PWA behavior with an immutable shell-only cache.
- A hardened Linux/systemd installer, a Docker Compose contract, and a Windows
  PowerShell installer for Docker Desktop Linux containers.
- Deterministic x86-64 and ARM64 Linux release archives with checksums,
  GitHub/Sigstore provenance, corresponding source archives, complete upstream
  attribution, bundled-asset notices, and a generated Rust dependency license
  inventory.

### Changed

- The reader identity, navigation, interface copy, static assets, and public
  documentation now consistently use Vale while retaining the `redlib` Cargo
  package/binary name for upstream compatibility.
- Process startup and local health no longer depend on successful Reddit OAuth
  acquisition; external retrieval recovers in the background.
- Removed the obsolete generic-web Reddit compatibility fallback after live
  verification showed that its upstream endpoint now rejects the flow. Vale
  retries the currently working installed-client flow without blocking local
  setup, login, or saved data.
- The default listener is loopback-only. Remote installer mode requires an
  exact HTTPS origin and secure cookies.
- Container installation now has one documented, CI-tested Debian image and
  Compose contract; unreferenced Alpine and Ubuntu variants were removed.
- A missing optional FFmpeg package now disables only video-download assembly
  instead of aborting the otherwise healthy Linux installation.

### Fixed

- Legacy `/r/COMMUNITY/w/PAGE` links retain their complete wiki page path when
  redirected to Vale's canonical local wiki route.

### Security

- Added native CSRF/origin checks, login throttling, secure session-cookie
  attributes, transactional password recovery, bounded request, OAuth,
  compression, decompression, archive queues, durable archive-record counts,
  and archive paths, plus account-private cache behavior.
- Restricted outbound redirects, media proxy destinations, response headers,
  and archive source capture to reviewed public destinations with size and
  timeout limits.
- Made the root Linux installer fail closed on symlinked or nonregular managed
  program, configuration, state, archive, cache, helper, unit, and binary
  paths. Runtime replacement is transactional across ordinary errors and
  termination signals, restores the prior active/enabled state, and never
  recursively changes ownership beneath a service-writable tree.
- Validated Reddit emote metadata structurally, restricted it to the exact
  reviewed asset path, bounded rendered dimensions, and ignored malformed
  entries without panicking.
- Minimized and pinned the locked dependency graph, added vulnerability and
  license gates, and pinned GitHub Actions to reviewed commit SHAs.
