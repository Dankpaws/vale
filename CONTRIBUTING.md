# Contributing to Vale

Thank you for helping improve Vale. It is a server-rendered Rust application
with local JavaScript enhancements, Askama templates, a profile database, and a
PWA shell. Keep changes small enough to review and preserve the Redlib engine's
attribution and AGPL-3.0 obligations.

## Before opening a change

1. Read the relevant section of `README.md`, `docs/INSTALL.md`,
   `docs/USAGE.md`, `docs/OPERATIONS.md`, and `HOMELAB.md`.
2. Search the current source for routes, settings, and models before adding a
   parallel implementation. `HOMELAB.md` defines behavior and safety
   boundaries; the source remains authoritative for symbol locations.
3. Decide whether the change affects account isolation, CSRF checks, cookies,
   caching, archives, service-worker behavior, responsive layouts, or the
   private-network boundary. These areas require focused verification.
4. Do not include production hostnames, internal addresses, account names,
   credentials, session values, private keys, or real private archives in a
   commit, screenshot, fixture, log, or issue.

## Local development

Install Rust 1.88 or newer (current stable is recommended) and the native
dependencies required by the `wreq`/BoringSSL build on your platform. Then run
the deterministic checks from the repository root:

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo test --locked
cargo build --release --locked --bin redlib
```

The executable is named `redlib` for upstream compatibility; the rendered
product is Vale. For a local account-mode smoke test, use a private temporary
database and archive directory and a loopback listener. Never point tests at a
production profile database.

Live Reddit/OAuth tests are ignored by default so ordinary CI is deterministic.
Run them deliberately with
`cargo test --locked -- --ignored --test-threads=1`, and report external OAuth,
rate-limit, DNS, or changing-Reddit-response failures separately from local
compiler, template, serialization, and unit-test failures.

## Behavior and security expectations

- Keep Vale's named-feed model separate; do not silently reintroduce All or
  Popular as normal destinations.
- Keep post titles and ordinary reading local to Vale. External source links
  must remain deliberate and clearly labeled.
- Preserve profile isolation for feeds, preferences, history, hidden posts, and
  archives. Test two accounts and two devices when changing profile behavior.
- Preserve `private, no-store` responses for dynamic/profile data and do not add
  profile or media responses to the service-worker shell cache casually.
- Keep account passwords out of configuration and logs. Preserve secure cookie
  attributes, session revocation, login throttling, and CSRF/origin checks.
- Keep archive capture bounded, profile-owned, restart-safe, and honest about
  partial results. Never execute captured external HTML or scripts.
- Use typography, borders, accent rules, spacing, and surface contrast for
  hierarchy. Do not add box shadows, text shadows, drop shadows, or hover
  elevation.
- Keep interactive controls keyboard-accessible, labeled, and touch-sized;
  maintain zero page-level horizontal overflow.

## Documentation changes

Installation instructions must work from a clean checkout and name every
prerequisite, privilege boundary, path, service/container command, health
check, first-run action, update path, backup requirement, and safe uninstall
step. Official release commands must name the canonical `Dankpaws/vale`
repository and an exact tag; do not substitute a floating release URL or an
unpublished image slug.

Feature documentation should explain purpose, normal operation, limits, and
failure state. If behavior is not implemented and verified, document it as
unfinished rather than presenting a decorative setting as functional.

Screenshots, diagrams, and recordings must use synthetic data and omit private
hostnames, addresses, credentials, cookies, tokens, and archive contents.
Provide alt text for visual aids and keep diagrams synchronized with the actual
request path.

## Pull-request checklist

- [ ] The change is limited to the stated behavior and does not revert
      unrelated work.
- [ ] `cargo fmt --check` passes.
- [ ] `cargo check --locked --all-targets` passes.
- [ ] `cargo test --locked` passes, with live-Reddit failures classified.
- [ ] `cargo build --release --locked --bin redlib` passes when runtime code
      changed.
- [ ] Focused browser or command-line verification covers the user-visible
      behavior, including a failure path where relevant.
- [ ] Account, cache, archive, CSRF, cookie, and network-boundary implications
      were reviewed when applicable.
- [ ] Public docs contain no secrets or private deployment details.
- [ ] Redlib attribution, AGPL-3.0 notices, and third-party credits remain
      intact.

## License

By contributing, you agree that your contribution is provided under the
repository's [GNU Affero General Public License v3.0](LICENSE). Preserve the
upstream Redlib [credits](CREDITS) and license text when redistributing.
