#!/usr/bin/env bash
# Build the browser module: target/web/ is a directory you can serve.
#
#   tools/web/build.sh            # release, wasm-bindgen'd, ready to serve
#   tools/web/shot.py             # …and run the bring-up probe in a headless browser
#
# The same module is both the bring-up probe (probe.html) and the player a web
# export ships (index.html + the game's bundle beside it). `target/web/` is
# also what File ⏵ Export Game… uses as the web template from a source
# checkout, when this engine version has no published one.
#
# Needs: `rustup target add wasm32-unknown-unknown`, wasm-bindgen-cli at the
# version Cargo.lock pins for wasm-bindgen, and the WASI SDK (tools/web/env.sh
# fetches it).
set -euo pipefail
cd "$(dirname "$0")/../.."
. tools/web/env.sh

want=$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p')
have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)
if [ "$have" != "$want" ]; then
    echo "wasm-bindgen-cli $want is required (have: ${have:-none}):" >&2
    echo "    cargo install wasm-bindgen-cli --version $want --locked" >&2
    exit 2
fi

target_dir=$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
out="$target_dir/web"

cargo build -p floptle-web --profile web --target wasm32-unknown-unknown
mkdir -p "$out"
wasm-bindgen --target web --no-typescript --out-dir "$out/pkg" \
    "$target_dir/wasm32-unknown-unknown/web/floptle_web.wasm"
cp crates/floptle-web/web/index.html crates/floptle-web/web/probe.html "$out/"
size=$(stat -c %s "$out/pkg/floptle_web_bg.wasm")
echo "built $out — floptle_web_bg.wasm is $((size / 1024)) KB"
