#!/usr/bin/env bash
#
# Fill every manifest entry's `title` and `notes` from `docs/releases/vX.Y.Z.md`.
#
#   scripts/embed-release-notes.sh releases.json [docs/releases]
#
# Rewrites the file in place. Versions with no matching doc are left exactly as
# they are, so this is safe to run over a manifest of any age.
#
# WHY THE NOTES TRAVEL WITH THE MANIFEST. The Hub shows a version's notes when
# you click it, including versions you already have installed and including the
# ones you are deciding between. Fetching a page per click needs a spinner, an
# error state, and a network connection to read about an engine already on the
# disk. One fetch at startup buys the whole history offline.
#
# WHY IT RUNS OVER EVERY VERSION AND NOT JUST THE NEW ONE. The manifest is
# accumulated — each release merges its entry into the previous list — so the
# 30-odd releases published before this existed carry no notes at all. Filling
# the whole list means they get backfilled by the next release without anybody
# remembering to do it, and it makes the script idempotent, which is what lets
# it be run by hand against a downloaded manifest to fix the history in one go.
#
# THE TITLE IS THE H1, and the notes are everything after it: the Hub draws its
# own heading from the version and the title, so leaving the `# Floptle v0.21.0
# — "Who's Playing"` line in the body would print the release name twice.
set -euo pipefail

MANIFEST="${1:?usage: embed-release-notes.sh <releases.json> [docs/releases]}"
DOCS="${2:-docs/releases}"

[ -f "$MANIFEST" ] || { echo "no such manifest: $MANIFEST" >&2; exit 1; }
[ -d "$DOCS" ] || { echo "no such notes directory: $DOCS" >&2; exit 1; }

filled=0
missing=()
work="$(mktemp)"
cp "$MANIFEST" "$work"

for ver in $(jq -r '.versions[].version' "$MANIFEST"); do
  doc="$DOCS/v$ver.md"
  if [ ! -f "$doc" ]; then
    missing+=("$ver")
    continue
  fi
  # The name in quotes on the H1, if it has one — `# Floptle v0.21.0 — "Who's Playing"`.
  title="$(head -n 1 "$doc" | sed -n 's/.*[“"]\(.*\)[”"].*/\1/p')"
  # Everything after the H1, with the blank line that followed it trimmed.
  body="$(tail -n +2 "$doc" | sed '/./,$!d')"
  jq --arg v "$ver" --arg t "$title" --arg n "$body" \
    '.versions |= map(if .version == $v then .title = $t | .notes = $n else . end)' \
    "$work" > "$work.next"
  mv "$work.next" "$work"
  filled=$((filled + 1))
done

mv "$work" "$MANIFEST"

echo "release notes: filled $filled of $(jq '.versions | length' "$MANIFEST")"
# Say what was skipped. A silent partial fill reads as "every version has notes"
# right up until somebody clicks the one that doesn't.
if [ ${#missing[@]} -gt 0 ]; then
  echo "no docs/releases/v<version>.md for: ${missing[*]}"
fi
