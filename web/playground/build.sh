#!/usr/bin/env bash
# Build the deployable playground bundle: web/playground/dist/ is everything noeta.dev's static
# host needs — the page, the worker, and the engine — plus brotli side-cars for hosts that serve
# precompressed assets (the ~4 MiB engine brotli-compresses to a fraction; a host without
# side-car support just ignores the .br files). Run from anywhere; needs the wasm32-unknown-unknown
# target (`rustup target add wasm32-unknown-unknown`).
set -euo pipefail
cd "$(dirname "$0")/../.."

cargo build -p noeta-playground --target wasm32-unknown-unknown --profile wasm-release

DIST=web/playground/dist
rm -rf "$DIST"
mkdir -p "$DIST"
cp web/playground/index.html web/playground/app.js web/playground/share.js web/playground/worker.js "$DIST/"
cp target/wasm32-unknown-unknown/wasm-release/noeta_playground.wasm "$DIST/"

if command -v brotli > /dev/null 2>&1; then
  for f in "$DIST"/*; do
    brotli --force --keep "$f"
  done
fi

echo "playground bundle ready: $DIST"
du -sh "$DIST"/noeta_playground.wasm* | sed 's/^/  /'
