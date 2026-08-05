#!/usr/bin/env bash
#
# Emit the docs feed the website renders: every published page, in order, with
# its markdown body.
#
#   scripts/build-docs-feed.sh <out.json> [docs] [version]
#
# WHY A FEED AND NOT A SCRAPE. The website could read these files straight out of
# the public repo. It would then show whatever is on `main` — work in progress,
# a half-written guide, a feature documented three weeks before it ships — beside
# a download button for a version that has none of it. Publishing the feed as a
# release asset ties the docs to the build they describe, exactly as
# `releases.json` ties the notes to the binaries.
#
# WHAT IT DELIBERATELY DOES NOT DO. It does not convert markdown to HTML, and it
# does not style anything. Where a heading sits and what a code block looks like
# is the website's business — it has a design, and this script has no business
# guessing at it. The feed is content plus structure; presentation stays W's.
#
# THE PAGE LIST IS NOT IN THIS SCRIPT. It is `docs/site-map.json`, which is also
# what the `every_doc_is_classified_for_the_website` test reads, so the site and
# the test can never disagree about what is published.
set -euo pipefail

OUT="${1:?usage: build-docs-feed.sh <out.json> [docs-dir] [version]}"
DOCS="${2:-docs}"
VERSION="${3:-$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')}"
MAP="$DOCS/site-map.json"

[ -f "$MAP" ] || { echo "no site map: $MAP" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

# The H1, minus the leading hash — the page's own title, so a rename does not
# need a second edit here.
title_of() {
  local f="$1" t
  t="$(grep -m1 '^# ' "$f" 2>/dev/null | sed 's/^# *//')"
  [ -n "$t" ] || t="$(basename "$f" .md)"
  printf '%s' "$t"
}

# The first real paragraph, flattened to one line: the summary a card or a nav
# hover shows. Skips the title, blockquote callouts, badges and fenced code.
summary_of() {
  awk '
    /^```/       { fenced = !fenced; next }
    fenced       { next }
    /^# /        { next }
    /^>/         { next }
    /^\[!\[/     { next }
    /^[[:space:]]*$/ { if (buf != "") exit; next }
    /^[#|<-]/    { if (buf != "") exit; next }
                 { buf = (buf == "" ? $0 : buf " " $0) }
    END          { print buf }
  ' "$1" | sed 's/[[:space:]]\+/ /g; s/^ //; s/ $//'
}

# Each page object is written to its own file and combined with --slurpfile:
# passing them on the command line blows ARG_MAX outright — lua-api.md alone is
# a few hundred KB, and `Argument list too long` is what you get for it.
pages_json() {
  local section="$1" dir="$2" n=0
  while IFS= read -r rel; do
    [ -n "$rel" ] || continue
    local f="$DOCS/$rel"
    if [ ! -f "$f" ]; then
      echo "site-map lists a missing page: $rel" >&2
      exit 1
    fi
    jq -n \
      --arg id "$(printf '%s' "$rel" | sed 's/\.md$//; s#/#-#g')" \
      --arg path "$rel" \
      --arg title "$(title_of "$f")" \
      --arg summary "$(summary_of "$f")" \
      --rawfile body "$f" \
      '{id: $id, path: $path, title: $title, summary: $summary, markdown: $body}' \
      > "$dir/$(printf '%04d' "$n").json"
    n=$((n + 1))
  done < <(jq -r --arg s "$section" '.sections[] | select(.id == $s) | .pages[]' "$MAP")
  # Concatenated in listed order, so the site's order is the map's order.
  cat "$dir"/*.json | jq -s '.'
}

sections_json() {
  local work="$1" first=1
  printf '['
  while IFS= read -r id; do
    [ -n "$id" ] || continue
    [ $first -eq 1 ] || printf ','
    first=0
    local pdir="$work/$id"
    mkdir -p "$pdir"
    pages_json "$id" "$pdir" > "$work/$id.pages.json"
    jq -n \
      --arg id "$id" \
      --arg title "$(jq -r --arg s "$id" '.sections[] | select(.id == $s) | .title' "$MAP")" \
      --arg blurb "$(jq -r --arg s "$id" '.sections[] | select(.id == $s) | .blurb' "$MAP")" \
      --slurpfile pages "$work/$id.pages.json" \
      '{id: $id, title: $title, blurb: $blurb, pages: $pages[0]}'
  done < <(jq -r '.sections[].id' "$MAP")
  printf ']'
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
sections_json "$WORK" > "$WORK/sections.json"

jq -n \
  --arg version "$VERSION" \
  --slurpfile sections "$WORK/sections.json" \
  '{schema: 1, version: $version, sections: $sections[0]}' > "$OUT"

pages="$(jq '[.sections[].pages[]] | length' "$OUT")"
bytes="$(jq '[.sections[].pages[].markdown | length] | add' "$OUT")"
# Guard against publishing an empty or half-built feed: a feed that parses but
# carries nothing renders as a docs site with no docs, and looks deliberate.
[ "$pages" -ge 50 ] || { echo "only $pages pages in the feed — refusing to publish it" >&2; exit 1; }
untitled="$(jq -r '[.sections[].pages[] | select(.title == "" or .summary == "")] | length' "$OUT")"
[ "$untitled" -eq 0 ] || { echo "$untitled page(s) have no title or no summary" >&2; exit 1; }

echo "$OUT: $pages pages, $(jq '.sections | length' "$OUT") sections, ${bytes} bytes of markdown, v$VERSION"
