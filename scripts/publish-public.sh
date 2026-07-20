#!/usr/bin/env bash
# Mirror a release from the private engine repo to the PUBLIC releases repo
# (Fopull-LLC/Floptle-releases) — the manual fallback for the release
# workflow's "Publish to the public releases repo" step when the
# RELEASES_TOKEN secret isn't configured. Needs a locally-authed `gh` with
# access to both repos.
#
#   scripts/publish-public.sh v0.1.1
set -euo pipefail

TAG="${1:?usage: publish-public.sh <tag>}"
PRIVATE="Fopull-LLC/Floptle"
PUBLIC="Fopull-LLC/Floptle-releases"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "downloading $TAG assets from $PRIVATE…"
gh release download "$TAG" --repo "$PRIVATE" --dir "$WORK"

echo "publishing $TAG on $PUBLIC…"
gh release create "$TAG" --repo "$PUBLIC" --title "$TAG" \
  --notes "Floptle $TAG — download the floptle-hub archive for your platform; the Hub installs engine versions." \
  2>/dev/null || true
gh release upload "$TAG" "$WORK"/* --clobber --repo "$PUBLIC"

echo "updating the manifest release…"
gh release create manifest --repo "$PUBLIC" --title "Release manifest" --prerelease \
  --notes "Machine-readable version manifest for the Floptle Hub — not a download page." 2>/dev/null || true
gh release upload manifest "$WORK/releases.json" --clobber --repo "$PUBLIC"
# Keep /releases/latest on the real bundles, never the manifest holder.
gh release edit manifest --repo "$PUBLIC" --prerelease
case "$TAG" in *-*) ;; *) gh release edit "$TAG" --repo "$PUBLIC" --latest;; esac

echo "done: https://github.com/$PUBLIC/releases/tag/$TAG"
