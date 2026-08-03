#!/usr/bin/env bash
#
# Fill every manifest entry's `title` and `notes` from `docs/releases/vX.Y.Z.md`.
#
#   scripts/embed-release-notes.sh releases.json [docs/releases] [docs/news.md]
#
# Rewrites the file in place. Versions with no matching doc are left exactly as
# they are, so this is safe to run over a manifest of any age.
#
# Also embeds `docs/news.md` as the manifest's top-level `news` — what the engine
# is working on and working towards, which the Hub shows on its News tab. Same
# reasoning as the notes: it rides a fetch that already happens, so it costs no
# request and reads from cache offline.
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

MANIFEST="${1:?usage: embed-release-notes.sh <releases.json> [docs/releases] [docs/news.md]}"
DOCS="${2:-docs/releases}"
NEWS="${3:-docs/news.md}"

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
  #
  # Via a FILE and `--rawfile`, not a shell variable and `--arg`. A single argv entry
  # is capped at 128 KB on Linux however big ARG_MAX is, and the failure — `Argument
  # list too long`, raised by the kernel before the program starts — names no argument
  # and appears only once something has grown past the limit. The release workflow lost
  # a build to exactly that; there is no reason for a second site to be able to.
  tail -n +2 "$doc" | sed '/./,$!d' > "$work.body"
  jq --arg v "$ver" --arg t "$title" --rawfile n "$work.body" \
    '.versions |= map(if .version == $v then .title = $t | .notes = $n else . end)' \
    "$work" > "$work.next"
  mv "$work.next" "$work"
  filled=$((filled + 1))
done
rm -f "$work.body"

# The news page, verbatim. `--rawfile` for the same reason as the notes above, and
# the H1 is dropped because the Hub draws its own heading for the tab.
if [ -f "$NEWS" ]; then
  sed '1{/^# /d;}' "$NEWS" | sed '/./,$!d' > "$work.news"
  jq --rawfile n "$work.news" '.news = $n' "$work" > "$work.next"
  mv "$work.next" "$work"
  rm -f "$work.news"
  echo "news: embedded $(wc -c < "$NEWS") bytes from $NEWS"
else
  # Never an error. A manifest with no news shows no news; a release that fails
  # because a prose file moved would be a worse trade than a quiet News tab.
  echo "news: no $NEWS — leaving the manifest's news field as it is"
fi

mv "$work" "$MANIFEST"

echo "release notes: filled $filled of $(jq '.versions | length' "$MANIFEST")"
# Say what was skipped. A silent partial fill reads as "every version has notes"
# right up until somebody clicks the one that doesn't.
if [ ${#missing[@]} -gt 0 ]; then
  echo "no docs/releases/v<version>.md for: ${missing[*]}"
fi
