#!/usr/bin/env bash
# The wasi:http serve e2e (P-WASM W4): `noeta build --serve` a real handler program, run the
# component under `wasmtime serve`, and assert a live HTTP round trip. Run by the CI `wasm` job
# (and by hand from the workspace root — needs the wasm32-wasip2 target and wasmtime on PATH).
set -euo pipefail
cd "$(dirname "$0")/../../.."

SCRATCH=$(mktemp -d)
SERVE_PID=""
trap 'rm -rf "$SCRATCH"; [ -n "$SERVE_PID" ] && kill "$SERVE_PID" 2>/dev/null || true' EXIT

cat > "$SCRATCH/edge.noe" <<'NOE'
use std.http.server
use std.http.{Request, Response}

fn handle(req: Request): Response {
    return server.response(200, "edge says hi: ${req.path()}")
}

server.serve(8080, handle)
NOE

# The lean (Cranelift-free) CLI drives the whole ladder: compile → bundle → cargo-bake the
# component. `--locked` is on the CLI build; the inner component build is the dev-tree path.
cargo run -q -p noeta-cli --no-default-features --locked -- \
  build --serve "$SCRATCH/edge.noe" -o "$SCRATCH/edge.serve.wasm"

ADDR=127.0.0.1:8917
wasmtime serve -S cli=y --addr "$ADDR" "$SCRATCH/edge.serve.wasm" &
SERVE_PID=$!
for _ in $(seq 1 100); do
  curl -s -o /dev/null "http://$ADDR/" && break
  sleep 0.2
done

BODY=$(curl -s "http://$ADDR/ping")
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "http://$ADDR/ping")
[ "$STATUS" = "200" ] || { echo "unexpected status: $STATUS"; exit 1; }
[ "$BODY" = "edge says hi: /ping" ] || { echo "unexpected body: $BODY"; exit 1; }
echo "wasi:http serve e2e: an unchanged http.serve program answered over real HTTP ✓"
