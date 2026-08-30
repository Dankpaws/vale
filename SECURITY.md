# Security and privacy

Vale is designed for a private deployment. It adds native Vale accounts and a
profile database to a Redlib-based reader, but it does not turn Reddit into a
private API or make a host safe to expose by itself. Read this document before
binding Vale to a non-loopback address.

## Threat model

Vale protects the following boundaries:

- Browser requests normally go to the Vale origin. Reddit media is fetched
  through a same-origin proxy rather than directly from the browser.
- Dynamic pages and profile responses are private and are not put in the
  service-worker shell cache. Proxied media can use private HTTP caching.
- Native account passwords are stored as salted Argon2id hashes. Session tokens
  are random values; only their SHA-256 hashes are stored.
- Session cookies use `Secure`, `HttpOnly`, `SameSite=Lax`, and the `__Host-`
  prefix when secure-cookie mode is enabled.
- Form mutations reject explicit cross-site requests and validate the expected
  origin.
- Profiles, history, hidden posts, feeds, and offline archives are isolated by
  account. Offline archives contain content deliberately saved by the reader
  and must be treated as sensitive data.

These controls do not protect a compromised host administrator, an exposed
backup, a browser that has already been compromised, or a user who deliberately
opens a source link outside Vale. The host operator remains responsible for
OS updates, firewall rules, HTTPS certificates, account administration, and
backups.

## Safe deployment defaults

1. Bind Vale to `127.0.0.1` for the first run.
2. Use a reverse proxy or another trusted TLS terminator for remote access.
3. Set `VALE_COOKIE_SECURE=on` and `REDLIB_FULL_URL` to the exact HTTPS origin.
4. Restrict the proxy with a private-network allowlist. Do not expose a fresh
   HTTP listener or a Docker port on all interfaces.
5. Run the process as a dedicated unprivileged user with restrictive state and
   cache permissions.
6. Keep the profile database and archives on protected, backed-up storage;
   keep disposable media cache separate.
7. Do not put Reddit credentials, OAuth tokens, session cookies, private keys,
   or passwords in source control, configuration, logs, screenshots, or bug
   reports.

The loopback-only HTTP setting used in local development is not an acceptable
remote deployment. If a browser cannot retain a login cookie over local HTTP,
that is a signal to use HTTPS for remote access, not to weaken the proxy or
firewall.

## Data handling

Vale retrieves public Reddit content through an unofficial installed-client
compatibility flow. Reddit can rate-limit or change that flow. Vale does not
require a Reddit account and does not store a Reddit password.

The Vale host can receive request metadata needed to serve the reader and can
store:

- usernames, display names, salted password hashes, and hashed sessions;
- feed membership, subscriptions, filters, preferences, history, and hidden
  post IDs; and
- saved post snapshots, comments, source addresses, media, hashes, and capture
  status.

The service worker caches only immutable shell assets. A preferences export is
not a database backup and does not contain the account password hash, sessions,
history, hidden posts, or archives. Protect exports and backups as private
data.

## Archive and external-content safety

Offline capture is bounded by item and total-store quotas, per-profile and
instance-wide metadata-record ceilings, comment and continuation limits, file
sizes, redirect counts, and external-resource rules.
External HTML is sanitized and retained as an inert best-effort snapshot;
Vale does not execute captured scripts. Do not open an archive from an
untrusted source or place it in a publicly served directory. The source link
on a live post can leave Vale and is subject to the source site's own security,
cookies, tracking, and content policy.

## Dependency-audit exception

The dependency audit reports **RUSTSEC-2025-0141** for `bincode 1.3.3`, which is
unmaintained. Vale retains this dependency only for compatibility reads of
legacy preference exports; it is not used for account or session storage. This
is not a claim that the dependency is maintained or vulnerability-free. Restore
input must remain strictly bounded in size and processing time, and new
preference exports use the revisioned `VAL1` format rather than introducing new
`bincode` data.

## Release-download trust policy

Official release archives always have a mandatory SHA-256 check in the Linux
installer. Passing `--github-repository Dankpaws/vale` additionally requires
GitHub/Sigstore attestation verification and fails closed when it cannot be
completed. Vale deliberately keeps checksum-only installation available for
minimal private hosts without a current GitHub CLI. That mode trusts the HTTPS
release origin and does not independently authenticate the publisher; it is an
accepted bootstrap compatibility tradeoff, not equivalent assurance. Release
maintainers must verify both the checksum and attestation before announcing an
official release.

## Reporting a vulnerability

Please do not publish credentials, session values, private URLs, raw archives,
or a working exploit in a public issue. Use the private vulnerability-reporting
form in the Security tab of [Dankpaws/vale](https://github.com/Dankpaws/vale/security)
when it is enabled, or use the contact method on the
[maintainer profile](https://github.com/Dankpaws). Include
only the minimum reproduction details needed to confirm the issue:

- the Vale release or commit under test;
- the installation path and operating-system family;
- a concise impact statement;
- reproducible steps using synthetic accounts and content; and
- any mitigation already applied.

Redact cookies, authorization headers, OAuth material, passwords, private keys,
internal addresses, account names, and archive contents. Allow maintainers
reasonable time to investigate and publish a fix before public disclosure.

## License and upstream notices

Vale is distributed under the [GNU Affero General Public License v3.0](LICENSE).
The Redlib engine remains credited and its upstream [license](LICENSE),
[credits](CREDITS), and applicable third-party notices must remain available
when Vale is redistributed.
