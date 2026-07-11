#!/usr/bin/env bash
# The wasi:http serve e2e (P-WASM W4): `noeta build --serve` a real handler program, run the
# component under `wasmtime serve`, and assert a live HTTP round trip. Run by the CI `wasm` job
# (and by hand from the workspace root — needs the wasm32-wasip2 target and wasmtime on PATH).
set -euo pipefail
cd "$(dirname "$0")/../../.."

SCRATCH=$(mktemp -d)
SERVE_PID=""
UPSTREAM_PID=""
trap 'rm -rf "$SCRATCH"; [ -n "$SERVE_PID" ] && kill "$SERVE_PID" 2>/dev/null; [ -n "$UPSTREAM_PID" ] && kill "$UPSTREAM_PID" 2>/dev/null || true' EXIT

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
kill "$SERVE_PID" 2>/dev/null; SERVE_PID=""
echo "wasi:http serve e2e: an unchanged http.serve program answered over real HTTP ✓"

# --- Outbound (W4 follow-up): the handler proxies a real upstream through the platform's
# wasi:http/outgoing-handler client. ---
python3 -c "
import http.server, threading, time
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'42 from upstream'
        self.send_response(200); self.send_header('content-length', str(len(body))); self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
s = http.server.ThreadingHTTPServer(('127.0.0.1', 8916), H)
threading.Thread(target=s.serve_forever, daemon=True).start()
time.sleep(120)
" &
UPSTREAM_PID=$!
sleep 0.5

cat > "$SCRATCH/proxy.noe" <<'NOE'
use std.http.server
use std.http.client
use std.http.{Request, Response}

fn handle(req: Request): Response {
    upstream = client.get("http://127.0.0.1:8916/data")
    return server.response(200, "edge proxied: ${upstream.body()}")
}

server.serve(8080, handle)
NOE
cargo run -q -p noeta-cli --no-default-features --locked --   build --serve "$SCRATCH/proxy.noe" -o "$SCRATCH/proxy.serve.wasm"
wasmtime serve -S cli=y --addr "$ADDR" "$SCRATCH/proxy.serve.wasm" &
SERVE_PID=$!
for _ in $(seq 1 100); do
  curl -s -o /dev/null "http://$ADDR/" && break
  sleep 0.2
done
BODY=$(curl -s "http://$ADDR/go")
[ "$BODY" = "edge proxied: 42 from upstream" ] || { echo "unexpected proxy body: $BODY"; exit 1; }
echo "wasi:http outbound e2e: the handler proxied a real upstream through outgoing-handler ✓"
