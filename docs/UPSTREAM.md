# Tracking upstream Redlib

Vale is a fork of [Redlib](https://github.com/redlib-org/redlib). Redlib still
provides Vale's Reddit retrieval and rendering engine, so upstream fixes,
especially changes that keep Reddit access working, need to reach Vale quickly.

## Fork point

Vale's public history starts from a reviewed clean export
([Publishing](PUBLISHING.md)), so it does not contain Redlib's commits and Git
cannot find a merge base on its own. The fork point is recorded in
[`UPSTREAM_BASE`](../UPSTREAM_BASE) instead. Comparing trees shows that the
initial snapshot (`7e2eb5c`) was taken from Redlib `main` at:

| Upstream commit | Date | Subject |
| --- | --- | --- |
| [`a4d36e9`](https://github.com/redlib-org/redlib/commit/a4d36e954cf1bd64f209cd8868c5a29edc81b374) | 2026-04-24 | Merge pull request #508 (RSS feed images), after #544 (switch to wreq) |

Of the recent upstream commits, this one has the smallest diff against Vale's
snapshot in the retrieval code (`src/oauth.rs`, `src/client.rs`, and the
listing/post parsers). Vale also uses its `wreq` client.

## Checking for upstream changes

```sh
scripts/upstream_status.sh
```

The script adds a local `upstream` remote when needed and fetches it without
tags. It then lists upstream commits since `UPSTREAM_BASE` and the files both
sides changed, which is where conflicts will be. Nothing is pushed.

## Bringing upstream changes into Vale

```sh
git switch -c sync/upstream-YYYY-MM-DD main
scripts/upstream_status.sh --apply
```

`--apply` applies the upstream diff since `UPSTREAM_BASE` with Git's three-way
fallback, then advances `UPSTREAM_BASE`. Vale's history gains one reviewed
commit authored by the maintainer identity. Upstream's commits are never
merged, which keeps the clean-history rule in `PUBLISHING.md`.

- Resolve conflicts in Vale's favor for account, archive, Saved, Reading,
  and interface behavior. Take upstream's side for Reddit retrieval: OAuth
  and client fingerprinting (`src/oauth.rs`, `src/oauth_resources.rs`,
  `src/client.rs`) and JSON parsing, unless Vale deliberately diverged there.
- Keep the Cargo package and binary name `redlib`: renaming it would turn
  every upstream change to `Cargo.toml` and `use redlib::...` into a conflict.
- Name the upstream range and pull requests in the commit message and under
  *Reddit compatibility* in `CHANGELOG.md`, then run the full check suite from
  `CONTRIBUTING.md`.

For a single upstream fix, apply just that commit's diff:
`git show <commit> | git apply --3way`. Leave `UPSTREAM_BASE` unchanged,
because the rest of the range is still pending.

Never push the `upstream` remote's refs, and never merge `upstream/main`.
Either would publish Redlib's full commit history and author identities.

## Upstream activity

As of 2026-09-30, upstream `main` has not changed since the fork point. With a
quiet upstream, Vale may be the first to notice a Reddit compatibility break.
See [Reddit access fallbacks](REDDIT_ACCESS.md) for where to go if the current
retrieval method stops working.
