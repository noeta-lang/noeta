#!/usr/bin/env bash
# deploy.sh — build the edge-hello handler and deploy it to Fermyon Spin.
#
# Two steps: `noeta build --serve` turns hello.noe into a wasi:http component, then Spin runs or
# deploys that component. Everything up to `spin deploy` works offline; the deploy step needs a
# Fermyon account and a logged-in Spin CLI, so it is guarded — a missing tool or login prints
# actionable instructions instead of a cryptic error.
#
# Usage:
#   ./deploy.sh          # build, then deploy (falls back to instructions if not logged in)
#   ./deploy.sh --local  # build, then serve locally with `spin up` (no account needed)
#   ./deploy.sh --build  # build only

set -euo pipefail
cd "$(dirname "$0")"

NOETA="${NOETA:-noeta}"
ARTIFACT="hello.serve.wasm"
MODE="${1:-deploy}"

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- 1. Build the component (always) --------------------------------------------------------
step "Building $ARTIFACT with noeta build --serve"
if ! have "$NOETA"; then
    echo "error: the 'noeta' CLI is not on PATH."
    echo "  Build it from the repo root with 'cargo build -p noeta-cli --bin noeta', or set"
    echo "  NOETA=/path/to/noeta and re-run. See docs/Getting-Started.md."
    exit 1
fi
"$NOETA" build --serve hello.noe -o "$ARTIFACT"
echo "built $ARTIFACT ($(wc -c < "$ARTIFACT") bytes) — a wasi:http/incoming-handler component"

[ "$MODE" = "--build" ] && exit 0

# --- 2. Check for the Spin CLI --------------------------------------------------------------
step "Checking for the Spin CLI"
if ! have spin; then
    cat <<'EOF'
error: the 'spin' CLI is not installed.

  Fermyon Spin runs and deploys wasi:http components. Install it:
    curl -fsSL https://spinframework.dev/downloads/install.sh | bash
    sudo mv ./spin /usr/local/bin/spin
  (or see https://spinframework.dev/install)

  Then re-run this script. To only produce the artifact, use './deploy.sh --build'.
EOF
    exit 1
fi
echo "found: $(spin --version)"

# --- 3a. Local serve (no account) -----------------------------------------------------------
if [ "$MODE" = "--local" ]; then
    step "Serving locally with spin up"
    echo "Serving on http://127.0.0.1:3000 — try:  curl http://127.0.0.1:3000/whoami"
    echo "Press Ctrl-C to stop."
    exec spin up
fi

# --- 3b. Deploy (needs a Fermyon account + login) -------------------------------------------
step "Deploying with spin deploy"
# `spin cloud` is the deploy plugin; a logged-out CLI has no valid token. Probe cheaply and,
# if not logged in, print the login instruction rather than letting `spin deploy` fail opaquely.
if ! spin cloud whoami >/dev/null 2>&1; then
    cat <<'EOF'
error: not logged in to Fermyon Cloud (or the 'cloud' plugin is missing).

  This is the one step that needs an account — it stays a manual action:
    1. Create a free account:  https://cloud.fermyon.com
    2. Install the deploy plugin (first time only):  spin plugins install cloud
    3. Log in:  spin login
    4. Re-run:  ./deploy.sh

  To deploy elsewhere, any host that speaks the wasi:http proxy world can serve
  hello.serve.wasm — see docs/Edge-Deployment.md.
EOF
    exit 1
fi

spin deploy
echo "deployed — Spin prints the public URL above."
