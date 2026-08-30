# Configuration and operations

This page is for the person who maintains a Vale instance after the first-run
setup. Keep the profile database and offline archives on a backed-up filesystem
and keep the disposable media cache separate. Never put passwords, session
cookies, OAuth values, or private keys in `redlib.toml`, a Compose file, a unit
file, or a support paste.

## Configuration model

Vale reads `redlib.toml` from its working directory and environment variables.
An environment variable wins when the same setting is present in both places.
`REDLIB_*` settings control the inherited Redlib engine; `VALE_*` settings
control Vale profiles, sessions, archives, and paths.

### Settings used by Vale

| Setting | Meaning | Safe guidance |
| --- | --- | --- |
| `VALE_PROFILE_MODE` | `browser`, `shared`, or `accounts` | Use `accounts` for a private multi-device instance. The first account-mode start opens one-time `/setup`. |
| `VALE_PROFILE_DATABASE` | SQLite profile database | Put it on durable storage readable and writable only by the Vale service user. |
| `VALE_ARCHIVE_DIR` | Permanent Saved/offline archive tree | Put it on durable storage separate from the disposable cache. |
| `VALE_MEDIA_CACHE_DIR` | Temporary video-download cache root (the runtime keeps its `video-downloads` child below this path) | Set the root in the service/container environment. It may be deleted and recreated; do not back it up as profile state. |
| `VALE_SESSION_DAYS` | Session lifetime, 1–365 days | The default is 30 days. Shorter is safer on a shared machine. |
| `VALE_COOKIE_SECURE` | Secure session-cookie mode | Keep `on` behind HTTPS. Use `off` only for loopback HTTP development. |
| `VALE_ARCHIVE_ITEM_MAX_BYTES` | Per-archive quota | Defaults to 1 GiB; accepted values are 64 MiB–4 GiB. |
| `VALE_ARCHIVE_TOTAL_MAX_BYTES` | Complete archive-store quota | Defaults to 2 GiB; accepted values are 1 MiB–64 GiB. A value below 256 MiB explicitly disables custom profile budgets. |

Saved-post metadata has non-configurable safety ceilings of 500 records per
profile and 5,000 records per instance. At admission time Vale may prune the
oldest failed retry record owned by the requesting profile, but it does not
automatically delete viewable archives or another profile's records. Reaching
either ceiling without an eligible failed row returns a storage-limit response;
remove an intended archive from **Saved** before retrying.

Each server-backed profile has an additive, revisioned archive-budget setting.
Zero means the instance maximum; nonzero values are whole 256 MiB steps no
larger than the configured instance maximum. Settings updates use optimistic
compare-and-swap, so two tabs cannot silently overwrite one another. Preference
exports include the value, but ordinary preference writes cannot replace the
dedicated setting accidentally.

Archive admission takes one immediate SQLite transaction, measures profile and
instance used bytes plus durable reservations, and records both the archive row
and a fixed reservation. At least 64 MiB must remain; each job preserves a
further 64 MiB finalization allowance. A cap decrease does not cancel already
admitted work. Publishing measures exact regular-file bytes after the final
directory is synced, then replaces the reservation with exact usage in one
transaction. Cleanup or deletion failure remains visible and counted. Startup
reconciles interrupted rows, reservations, partial/final directories, deletion
tombstones, and exact totals before accepting new work; unexpected archive-root
entries fail startup closed instead of being ignored.

### Useful engine defaults

| Setting | Meaning |
| --- | --- |
| `REDLIB_FULL_URL` | Canonical browser origin, such as `https://vale.example.test`; use the exact scheme and host, with no path. |
| `REDLIB_DEFAULT_THEME` | Initial theme; Vale supports System, Light, and Dark. |
| `REDLIB_DEFAULT_SUBSCRIPTIONS` | Initial followed communities for a new profile, separated with `+`; do not ship personal communities in a public release. |
| `REDLIB_DEFAULT_REMOVE_DEFAULT_FEEDS` | Keep `on` to omit inherited All/Popular defaults from the normal Vale navigation. |
| `REDLIB_ROBOTS_DISABLE_INDEXING` | Keep `on` for a private instance. |
| `REDLIB_ENABLE_RSS` | Leave `off` unless RSS is intentionally supported and documented. |

The application itself forces the Vale reader contract after loading older
preferences: one responsive layout, three theme intents, separate named feeds,
instant hide with Undo, and the current keyboard defaults. Do not expose an
environment switch in documentation unless the released code both implements
and verifies it.

## Where state belongs

Choose paths that are private to the service account and include them in the
backup plan:

| Installation | Profile database and archives | Disposable cache |
| --- | --- | --- |
| Linux service | The installer-created state directory, commonly `/var/lib/vale` | The installer-created cache directory, commonly `/var/cache/vale` |
| Linux source smoke test | A private directory under the operator's home | A private cache directory under the operator's home |
| Windows Docker Desktop | The durable Compose volume mounted at `/var/lib/vale` | The separate Compose volume mounted at `/var/cache/vale` |
| Windows WSL2 | The Linux service/source paths inside the WSL2 distribution | The matching private Linux cache path |
| Docker | The durable volumes mounted at `/var/lib/vale` | The separate volume mounted at `/var/cache/vale` |

Do not use a synchronized consumer folder for the live SQLite database. Do not
mount the same profile database into two writable Vale processes. SQLite uses a
WAL sidecar during normal operation; a file copy that omits the matching WAL
and shared-memory files may be inconsistent.

## Backups and recovery

Back up the profile database and archive tree together. The media cache is
optional and can be rebuilt. Encrypt backups and restrict their permissions;
they contain account metadata, reading history, hidden posts, and any content
the reader saved for offline use.

The consistent boundary includes `post_archives`, `profile_archive_settings`,
`archive_reservations`, and the complete archive tree. Copying only completed
directories while omitting the matching SQLite snapshot can lose in-flight
reservations or deletion tombstones; copying only SQLite can leave exact byte
accounting without its files.

### Linux service or source process

1. Record the installed Vale version and configuration file path without
   recording its values in a public issue.
2. Use SQLite's backup operation or stop the process before copying the
   database. For a stopped process, copy the database and its archive directory
   as one protected backup set.
3. Verify the backup by checking its file sizes and opening a copy of the
   database with `sqlite3` using `PRAGMA quick_check;`.
4. Keep at least one backup away from the Vale host.

For a running service, a SQLite-aware backup is preferable to a blind `cp` of
only `profiles.sqlite3` because WAL data may still be pending. If `sqlite3` is
not installed, stop the service first and copy the complete database file set.

### Docker

Use the backup facility of the Docker host or Docker Desktop for the named
volumes. Confirm that the backup contains both the profile database and the
archive tree. `docker compose down` does not remove volumes; never use
`docker compose down --volumes` as a routine update or diagnostic command.
Restore into the same durable volumes while the container is stopped, then run
the health and sign-in checks in [Installation](INSTALL.md#after-installation-a-complete-smoke-test).

### Restore procedure

1. Stop Vale and prevent a second instance from writing the same state.
2. Preserve the current state as a rollback copy before changing anything.
3. Restore the database and archives together, preserving ownership, ACLs, and
   directory layout.
4. Start Vale and check `/healthz`, the login page, the expected account, a
   named feed, and one saved archive.
5. Change passwords or revoke sessions if the backup may have been exposed.

A preferences export is not a profile backup. It can restore reader settings,
but it does not restore accounts, password hashes, sessions, history, hidden
posts, or offline archives.

If the only administrator password is lost, Vale has no email-based password
recovery. Use the built-in local recovery command below; if that command is
not available in the installed release, stop the service and use a
maintainer-approved offline recovery procedure. Do not paste a password hash,
edit the live database ad hoc, or disable account mode as a shortcut.

### Reset an account password from the local binary

When no administrator can sign in, use the built-in local recovery command. It
works only in `accounts` mode and uses two hidden TTY prompts for the new
password without putting it in the command line, shell history, logs, or an
environment file.
The update is transactional and revokes every existing session for that
account. Stop the running Vale process first so only the recovery command
writes the SQLite database.

On a Linux system-service install, use the service's profile database path
(the installer default is shown here):

```sh
sudo systemctl stop vale.service
sudo /usr/local/bin/vale-admin admin reset-password --username ACCOUNT
sudo systemctl start vale.service
```

Replace `ACCOUNT` with the exact local username. For a source build, stop the
foreground process and run the same command with
`./target/release/redlib` and the database path used by that process. WSL2
uses this Linux procedure inside the distribution.

For Docker Compose—including the Windows PowerShell installer—stop the `vale`
service, run the one-off command against its existing durable volumes, and
start it again:

```sh
docker compose config --services
docker compose stop vale
docker compose run --rm --no-deps vale admin reset-password --username ACCOUNT
docker compose start vale
```

If the image does not make the Vale binary its entrypoint, add the release's
documented binary path with Compose's `--entrypoint` option. Confirm the
container uses the same profile volume and `VALE_PROFILE_DATABASE` as the
stopped service; never run recovery against a disposable container filesystem.

## Updating safely

Use one pinned release at a time:

1. Read the release notes and confirm the artifact checksum and published
   GitHub/Sigstore provenance when using an official binary archive.
2. Make and verify a profile/archive backup.
3. Record the current binary/image version and configuration paths.
4. Stop the service or container before replacing the artifact.
5. Install the new pinned artifact without changing the state paths.
6. Start Vale, check `/healthz`, inspect logs for startup errors, and sign in.
7. Verify a named feed, an eligible Hide/Undo replenishment, a community rail,
   a local post, and a new Reader-v3 archive before allowing remote traffic
   again. Confirm an older Reader-v1 or Reader-v2 archive still serves without
   manifest rewrite.

For a source build, also run `node --test tests/js/*.test.mjs`. The deterministic
client suite covers keyed listing reconciliation, mutation ordering and
uncertainty recovery, cross-tab sequence handling, BFCache gates, mobile feed
geometry, settings dirty boundaries, and the bounded Undo shell policy. Keep
the live-Reddit assertions classified separately from these local checks.

For the checked-in Docker contract, switch the checkout to the exact reviewed
tag, run `docker compose build --pull`, and then `docker compose up -d`. The
Compose file builds Vale locally and does not claim a public registry image.
Do not run `docker compose down --volumes` during an update. The Linux installer
stages the new runtime controls before it runtime-masks and stops the old unit.
It keeps private transaction copies of the prior binary, service unit, and
admin helper until the new service is healthy, and restores their exact prior
active/enabled state after any ordinary error, `Ctrl+C`, or termination signal.
Configuration, profile data, archives, and cache are never part of that runtime
rollback. If restoration itself fails, the installer deliberately leaves the
unit stopped and runtime-masked for operator repair and retains the exact
root-only transaction directory it prints under `/var/tmp`. Do not delete that
directory until the recorded controls are reconciled. A manual binary rollback
should likewise keep the newer profile database and archives unless a
separately approved, consistent whole-profile restore is required.

The Linux installer's listener and origin options are first-install choices.
When `/etc/vale/vale.env` already exists, an update preserves it and rejects
`--bind-address`, `--port`, `--cookie-secure`, and `--public-url` (and their
matching environment variables) instead of pretending to apply them. For a
planned reconfiguration, back up and edit that protected file directly, then
restart Vale and verify its local health, external HTTPS origin, secure cookie,
and narrow firewall path before restoring access.

The installer also refuses a symlink or non-directory at any managed program,
configuration, state, archive, or cache directory, and refuses symlinked
configuration, helper, unit, or binary files. Treat that failure as a local
integrity warning: inspect the exact path without following it, restore the
expected ownership from a trusted backup, and retry only after determining why
the managed path changed. Root creates only the state/cache boundary entries
whose names are protected by root-owned parents. Service-owned child
directories are created as `vale`, and the installer never recursively changes
ownership below a tree writable by the service.

## Uninstalling without deleting data

Uninstalling the program and deleting user data are separate operations.

- **Linux:** stop and disable the Vale service, remove the executable and unit
  only after confirming the paths, and retain the state directory until the
  owner has an encrypted backup.
- **Windows Docker Desktop:** run `docker compose down`, remove the checkout or
  image only after confirming the backup, and retain both named volumes.
- **Windows WSL2:** follow the matching Linux uninstall path inside WSL2 and
  retain its protected state directory.
- **Docker:** run `docker compose down`, remove the checkout or image only
  after confirming the backup, and leave volumes intact. Delete volumes only
  after explicitly deciding to destroy profiles and archives.

To permanently destroy data, identify the exact database, archive, cache,
backup, and volume paths first and obtain a separate confirmation. A broad
recursive delete is not an uninstall procedure.

## Logs and health checks

The unauthenticated health endpoint should return exactly `ok` when the process
and profile database can initialize:

```sh
curl --fail http://127.0.0.1:8080/healthz
```

Record the package version and embedded build revision without dumping the
environment or configuration:

```sh
# Linux installer
/usr/local/lib/vale/vale --version

# Running Compose service
docker compose exec -T vale /usr/local/bin/vale --version
```

An official release build reports its reviewed source revision. A build made
from an unpacked source tree without Git metadata reports `dev`; do not present
that label as proof of a published commit.

Use the command for the installation you chose:

```sh
# Linux system service installed by install.sh
systemctl status vale.service
journalctl -u vale.service --since "15 minutes ago" --no-pager

# Docker Compose
docker compose ps
docker compose logs --tail=200 vale
```

On Windows Docker Desktop, use the same Compose commands from PowerShell. WSL2
uses the Linux commands inside its distribution.

Do not include full logs in a public issue until you have removed cookies,
authorization headers, OAuth material, private paths, account names, and saved
content. A small status line and the Vale version are usually enough to begin
triage.

## Troubleshooting

| Symptom | Likely cause | Safe next step |
| --- | --- | --- |
| Browser cannot connect | Vale is stopped, the port is occupied, or the listener is loopback-only | Check service/container status, `curl /healthz`, and the configured port. Keep loopback unless private HTTPS ingress is ready. |
| `/setup` never appears | An account already exists, account mode is not enabled, or the configured database is not the one the process opened | Confirm `VALE_PROFILE_MODE=accounts`, the working directory/config path, and the database path. Do not delete the database to force setup. |
| Login immediately returns to login over local HTTP | `VALE_COOKIE_SECURE=on` is correct for HTTPS but cannot persist a cookie over plain HTTP | For loopback development only, set it to `off`; for every remote deployment, use HTTPS and set it back to `on`. |
| Startup reports database permission errors | The service user cannot create or write the profile database/archive directory | Fix ownership and ACLs on the exact configured paths. Do not run Vale as root. |
| Docker restart loses accounts or Saved items | The profile database/archive paths are not mounted on durable volumes | Stop the container, inspect `docker compose config`, restore from backup, and correct the mounts before logging in again. |
| Feeds or posts show a Reddit/OAuth/rate-limit error | Reddit changed or throttled its unofficial retrieval path, or outbound HTTPS/DNS is blocked | Check external connectivity and logs without exposing tokens. Retry later and classify the result separately from local health. |
| Video playback works but download fails | FFmpeg is missing, not executable, or the stream exceeded the bounded download limit | Install the release-supported FFmpeg package or mark video download unavailable. Do not remove the size/time bounds. |
| Video shows the audio-aware Retry panel | HLS is disabled, the browser/player could not prepare the playlist, or the selected stream did not advertise audio | Enable HLS in Settings and Retry. Apple browsers use their native HLS engine; other browsers use the bundled player and fall back to native HLS if media attachment stalls. If the panel remains, inspect the same-origin HLS response and browser support; do not add a silent MP4 fallback. |
| Saved reports a partial archive | Reddit returned incomplete comments, an external page was sanitized, a resource was unavailable, or a configured quota was reached | Open the capture report and read its omissions. A partial archive is not a corrupted account; retry only after checking storage and network. |
| Hide succeeds but a listing says Retry | Vale saved or verified the hidden state, but the bounded four-page snapshot could not complete | Keep the current cards, use the explicit Retry control, and check Reddit availability. Do not clear hidden state or broaden the listing bound as a workaround. |
| Saved reports cleanup or removal needs attention | A partial/final directory could not be removed or synced durably | Preserve the row and files, correct the exact filesystem problem, and restart for reconciliation. Do not delete the accounting row by hand. |
| PWA looks stale after an update | The browser retained an immutable shell asset from the previous release | Reload from the final HTTPS origin, then use the browser's site-data/PWA update controls. Do not expect the service worker to cache feeds or offline archives. |
| Remote clients cannot connect | Reverse proxy, private firewall, DNS, or certificate does not match the configured origin | Test from the intended private network, verify `Host`/`X-Forwarded-Proto`, certificate SNI, and `REDLIB_FULL_URL`. Do not open a broad public firewall rule. |

## Operational safety checklist

- [ ] HTTPS is enabled before remote access, and `VALE_COOKIE_SECURE=on`.
- [ ] The application port is loopback-only or restricted by a private firewall.
- [ ] The process runs as an unprivileged account.
- [ ] The profile database and archives are on protected durable storage.
- [ ] The media cache is separate and disposable.
- [ ] Backups are encrypted, tested, and stored away from the host.
- [ ] Updates are pinned and preceded by a backup.
- [ ] `docker compose down --volumes` is not used casually.
- [ ] Logs and screenshots are scrubbed before sharing.
- [ ] No Reddit credentials, OAuth token, session cookie, or private key is
      present in configuration or documentation.
