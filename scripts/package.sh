#!/usr/bin/env bash
# Build a self-contained Floptle editor bundle for THIS platform, plus a local
# releases.json the Hub can install from via its LocalBuilds/manifest source — the offline
# dev loop for the Hub (ADR-0021 / docs/hub-proposal.md). The CI (.github/workflows/
# release.yml) does the same across all OSes for real releases.
#
# Usage:  VERSION=0.1.0 scripts/package.sh          # OUT defaults to dist/
#         VERSION=0.1.0 OUT=/tmp/floptle scripts/package.sh
#
# Then in the Hub → Settings, set "Manifest URL" to <OUT>/releases.json and install.
set -euo pipefail

VERSION="${VERSION:-0.0.0-dev}"
OUT="${OUT:-dist}"

case "$(uname -s)" in
  Linux)  OS=linux ;;
  Darwin) OS=macos ;;
  *) echo "unsupported host OS (use the CI for Windows bundles)"; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  ARCH=x86_64 ;;
  arm64|aarch64) ARCH=aarch64 ;;
  *) echo "unsupported arch $(uname -m)"; exit 1 ;;
esac
TARGET="$OS-$ARCH"   # must match floptle_hub::releases::platform_target()

echo "==> building floptle-editor (release)"
cargo build --release -p floptle-editor

# Respect a custom CARGO_TARGET_DIR / .cargo/config target-dir (portable, no jq needed).
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)"
TARGET_DIR="${TARGET_DIR:-target}"
# The floptle-editor crate's [[bin]] is named `floptle`.
BIN="$TARGET_DIR/release/floptle"
[ -f "$BIN" ] || { echo "editor binary not found at $BIN"; exit 1; }

mkdir -p "$OUT"
BUNDLE="$OUT/floptle-$VERSION-$TARGET"
rm -rf "$BUNDLE"; mkdir -p "$BUNDLE"
cp "$BIN" "$BUNDLE/floptle"
COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
printf '{ "version": "%s", "target": "%s", "commit": "%s" }\n' "$VERSION" "$TARGET" "$COMMIT" \
  > "$BUNDLE/version.json"

# Flat archive: bundle CONTENTS at the archive root (matches the Hub's unpack).
ARCHIVE="$OUT/floptle-$VERSION-$TARGET.tar.gz"
tar -C "$BUNDLE" -czf "$ARCHIVE" .

if command -v sha256sum >/dev/null 2>&1; then SHA=$(sha256sum "$ARCHIVE" | cut -d' ' -f1)
else SHA=$(shasum -a 256 "$ARCHIVE" | cut -d' ' -f1); fi
SIZE=$(wc -c < "$ARCHIVE" | tr -d ' ')
ABS="$(cd "$(dirname "$ARCHIVE")" && pwd)/$(basename "$ARCHIVE")"
echo "==> $ARCHIVE  ($SIZE bytes, sha256 $SHA)"

# A local manifest whose artifact URL is the local archive path — the Hub copies + verifies
# + unpacks it (no network). Point Settings → Manifest URL here.
cat > "$OUT/releases.json" <<JSON
{
  "schema": 1,
  "channels": { "stable": ["$VERSION"] },
  "versions": [
    {
      "version": "$VERSION",
      "channel": "stable",
      "date": "$(date +%F)",
      "artifacts": {
        "$TARGET": { "url": "$ABS", "sha256": "$SHA", "size": $SIZE }
      }
    }
  ]
}
JSON
echo "==> wrote $OUT/releases.json"
echo "    In the Hub → Settings, set Manifest URL to: $(cd "$OUT" && pwd)/releases.json"
