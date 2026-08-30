#!/usr/bin/env bash
#
# Check `docs/releases/scope.json` against what a release actually changed.
#
#   scripts/check-scope.sh [version] [previous-tag]
#
# Defaults to the workspace version in Cargo.toml, compared against the newest tag
# below it. Exit 0 when the declaration matches the diff, 1 when it does not.
#
# WHY THIS EXISTS. One tag builds both binaries, so `scope.json` is the ONLY thing
# that says which of them a release actually changed — and it is written by hand.
# For ~90 releases a wrong line there was merely noise: the Hub ignored the field
# when checking for its own updates and offered a self-update on every engine
# release. Now that `hub_update_available` honours it, a wrong line is worse than
# noise in the other direction — declare an engine-only release that really did fix
# the Hub, and that fix never reaches anybody. This is the check that catches it.
#
# WHAT COUNTS AS "THE HUB CHANGED". The Hub links eight workspace crates, but six of
# them are the engine — `floptle-core` and `floptle-scene` change on most releases,
# and treating those as Hub changes would mark every release "both" and put the nag
# straight back. The Hub's OWN code is `floptle-hub`, `floptle-dist` (the manifest
# and the update client) and `floptle-account`. Measured over the ten releases before
# this script: those three moved on exactly one, v0.79.0, which is the one release in
# that window that genuinely shipped Hub changes.
#
# `floptle-scene` is reported as an advisory rather than a failure. The Hub reads and
# writes `project.ron` through it, so a change there CAN be Hub-facing — but it is an
# engine crate and it is usually an engine change, and only a human knows which.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SCOPE="docs/releases/scope.json"
HUB_CRATES=(crates/floptle-hub crates/floptle-dist crates/floptle-account)

VERSION="${1:-$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)}"
[ -n "$VERSION" ] || { echo "could not read a version from Cargo.toml" >&2; exit 1; }

# The newest tag strictly below this version — what "since the last release" means.
PREV="${2:-$(git tag --list 'v*' | sed 's/^v//' | sort -V \
  | awk -v v="$VERSION" '$0 != v' | awk -v v="$VERSION" '
      { split($0, a, "."); split(v, b, ".");
        for (i = 1; i <= 3; i++) { if (a[i] + 0 < b[i] + 0) { print; next }
                                   if (a[i] + 0 > b[i] + 0) next } }' \
  | tail -1 | sed 's/^/v/')}"
[ -n "$PREV" ] || { echo "no tag below v$VERSION to compare against" >&2; exit 1; }
git rev-parse -q --verify "$PREV^{commit}" >/dev/null \
  || { echo "no such tag: $PREV" >&2; exit 1; }

# HEAD when the version is not tagged yet — which is the case that matters, because
# this is meant to run BEFORE the tag goes up.
HEAD_REF="v$VERSION"
git rev-parse -q --verify "$HEAD_REF^{commit}" >/dev/null || HEAD_REF="HEAD"

hub_files="$(git diff --name-only "$PREV..$HEAD_REF" -- "${HUB_CRATES[@]}")"
scene_files="$(git diff --name-only "$PREV..$HEAD_REF" -- crates/floptle-scene)"

# Uncommitted Hub work counts too. The tag goes on a commit, so the committed range is
# the real answer — but this is meant to be run WHILE preparing a release, when the
# change being released may still be in the working tree, and answering "no" there is
# the exact mistake the script exists to prevent.
# …but only when the version is NOT tagged yet. Checking a historical release must not
# pull in whatever happens to be dirty today.
uncommitted=""
[ "$HEAD_REF" = HEAD ] && uncommitted="$(git status --porcelain -- "${HUB_CRATES[@]}" | sed 's/^...//')"
if [ -n "$uncommitted" ]; then
  hub_files="$(printf '%s\n%s' "$hub_files" "$uncommitted" | grep -v '^$' | sort -u)"
fi

if [ -n "$hub_files" ]; then actual_hub=yes; else actual_hub=no; fi

# Absent means "both", which is the safe default and the normal case.
if [ -f "$SCOPE" ] && declared="$(jq -r --arg v "$VERSION" '.[$v] // empty | join(",")' "$SCOPE")" \
   && [ -n "$declared" ]; then
  case ",$declared," in *,hub,*) declared_hub=yes ;; *) declared_hub=no ;; esac
else
  declared="(absent — means both)"
  declared_hub=yes
fi

echo "v$VERSION vs $PREV ($HEAD_REF)"
echo "  declared: $declared"
echo "  hub crates changed: $actual_hub$([ -n "$hub_files" ] && echo " ($(echo "$hub_files" | wc -l) files)")"
[ -n "$hub_files" ] && echo "$hub_files" | sed 's/^/    /'
[ -n "$uncommitted" ] && echo "  (includes uncommitted working-tree changes)"
if [ -n "$scene_files" ]; then
  echo "  advisory: floptle-scene changed ($(echo "$scene_files" | wc -l) files) — engine crate the Hub also links,"
  echo "            call it yourself if the change is Hub-facing (project.ron read/write)."
fi

if [ "$declared_hub" = "$actual_hub" ]; then
  echo "  OK"
  exit 0
fi

echo
if [ "$actual_hub" = yes ]; then
  cat <<MSG
MISMATCH: this release changed the Hub's own crates, but scope.json says it did not.
Every installed Hub will skip this version — the fix ships and nobody is offered it.
Fix: make the "$VERSION" line include "hub", or drop the line entirely (absent = both).
MSG
else
  cat <<MSG
MISMATCH: scope.json says this release changed the Hub, and no Hub crate moved.
Every installed Hub will offer a self-update for a binary that did not change.
Fix: set the "$VERSION" line to ["engine"].
MSG
fi
exit 1
