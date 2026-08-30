# Changelog

This file records user-visible Vale changes. Release entries use calendar dates
and immutable Git tags; externally caused Reddit compatibility changes are
called out separately from local regressions.

## Unreleased

No user-visible changes have been recorded since 0.36.0.

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
