#!/usr/bin/env bash
# Compare Vale with upstream Redlib from the recorded fork point.
#
#   scripts/upstream_status.sh            report pending upstream commits
#   scripts/upstream_status.sh --apply    apply them to the working tree
#
# Vale's public history deliberately does not contain Redlib's history (see
# docs/PUBLISHING.md), so upstream changes are carried over as a three-way
# applied diff from the commit named in UPSTREAM_BASE, never as a merge.
# The `upstream` remote is added locally when missing; nothing is pushed.
set -euo pipefail

upstream_url="https://github.com/redlib-org/redlib.git"
ref="upstream/main"
apply=0
case "${1:-}" in
  "") ;;
  --apply) apply=1 ;;
  *) echo "usage: $0 [--apply]" >&2; exit 2 ;;
esac

cd "$(git rev-parse --show-toplevel)"
base="$(tr -d '[:space:]' < UPSTREAM_BASE)"

if ! git remote get-url upstream >/dev/null 2>&1; then
  git remote add upstream "$upstream_url"
fi
# Keep upstream refs out of `git push --tags` and wildcard pushes.
git fetch --quiet --no-tags upstream main

if ! git cat-file -e "$base^{commit}" 2>/dev/null; then
  echo "UPSTREAM_BASE $base is not an upstream commit." >&2
  exit 1
fi

echo "Recorded fork point: $(git log -1 --format='%h %ad %s' --date=short "$base")"
pending="$(git rev-list --count "$base..$ref")"
echo "Upstream commits since then: $pending"
if [[ "$pending" -eq 0 ]]; then
  exit 0
fi

echo
git log --no-merges --format='  %h %ad %an: %s' --date=short "$base..$ref"
echo
echo "Files changed on both sides (likely conflicts):"
comm -12 \
  <(git diff --name-only "$base" "$ref" | sort) \
  <(git diff --name-only "$base" HEAD | sort) | sed 's/^/  /'

if [[ "$apply" -eq 1 ]]; then
  if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "Commit or stash local changes before applying upstream changes." >&2
    exit 1
  fi
  target="$(git rev-parse "$ref")"
  echo
  echo "Applying $base..${target:0:7} with three-way fallback..."
  if git diff --binary "$base" "$target" | git apply --3way --index; then
    echo "$target" > UPSTREAM_BASE
    git add UPSTREAM_BASE
    echo "Applied cleanly. Review, run the full check suite, and commit."
  else
    echo "Some hunks conflicted; resolve the marked files, then set UPSTREAM_BASE to $target." >&2
    exit 1
  fi
fi
