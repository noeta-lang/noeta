#!/usr/bin/env bash
# Keyless-signing smoke test against the Sigstore STAGING instance (Phase 5, K5).
#
# Maintainer-run, NOT part of the test suite: it talks to real external services
# (fulcio.sigstage.dev / rekor.sigstage.dev) and needs a real ambient OIDC identity, so run it
# from a CI job (e.g. a manually-dispatched GitHub Actions workflow with `id-token: write`).
# Everything hermetic about the keyless path is already covered in-repo by the
# `keyless-test-fixtures` tests; this script's only job is to catch drift between our clients
# and the real services' wire behavior.
#
# Requires: the `noeta` binary on PATH (or NOETA=path), git, an ambient OIDC identity, and a
# staging trust root JSON (pass its path as $1 — export it from Sigstore's staging TUF
# repository, e.g. with `cosign trusted-root export --staging`).
set -euo pipefail

NOETA="${NOETA:-noeta}"
STAGING_TRUST_ROOT="${1:?usage: keyless-staging-smoke.sh <staging trusted_root.json>}"

if [[ "${GITHUB_ACTIONS:-}" != "true" && -z "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ]]; then
    echo "warning: no ambient OIDC identity detected — publish will likely fall back or fail" >&2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
echo "workdir: $work"

# A throwaway tagged package repo + an isolated local registry index.
repo="$work/pkg" reg="$work/registry" app="$work/app"
mkdir -p "$repo" "$reg" "$app"
git -C "$repo" init -q
cat > "$repo/noeta.toml" <<'EOF'
[package]
name = "noeta-smoke/keyless"
version = "0.0.1"
EOF
cat > "$repo/m.noe" <<'EOF'
namespace keyless.core;
pub fn v(): int { return 1; }
EOF
git -C "$repo" add . && git -C "$repo" -c user.email=smoke@noeta.dev -c user.name=smoke commit -qm smoke
git -C "$repo" tag v0.0.1

echo "== keyless publish against Sigstore STAGING =="
( cd "$repo" && \
  NOETA_REGISTRY_DIR="$reg" \
  NOETA_FULCIO_URL="https://fulcio.sigstage.dev" \
  NOETA_REKOR_URL="https://rekor.sigstage.dev" \
  NOETA_SIGSTORE_TRUST_ROOT="$STAGING_TRUST_ROOT" \
  "$NOETA" publish --git "$repo" --tag v0.0.1 | tee "$work/publish.out" )
grep -q "keyless:" "$work/publish.out" || { echo "FAIL: publish was not keyless"; exit 1; }

echo "== consumer resolve + offline verification + identity pin =="
cat > "$app/noeta.toml" <<'EOF'
[package]
name = "noeta-smoke/app"
version = "0.0.1"
[dependencies]
k = { version = "^0.0.1", package = "noeta-smoke/keyless" }
EOF
echo 'echo 42;' > "$app/main.noe"
NOETA_REGISTRY_DIR="$reg" \
NOETA_SIGSTORE_TRUST_ROOT="$STAGING_TRUST_ROOT" \
"$NOETA" run "$app/main.noe" | grep -qx 42 || { echo "FAIL: consumer resolve"; exit 1; }
grep -q 'identity = ' "$app/noeta.lock" || { echo "FAIL: no keyless pin in noeta.lock"; exit 1; }

echo "== audit =="
NOETA_REGISTRY_DIR="$reg" NOETA_SIGSTORE_TRUST_ROOT="$STAGING_TRUST_ROOT" \
"$NOETA" audit "$app" | grep -i keyless

echo "OK: staging smoke passed — publish, offline verify, TOFU pin, audit"
