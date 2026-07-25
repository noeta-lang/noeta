#!/bin/sh
# Noeta installer — downloads the released `noeta` binary for this machine and puts it on PATH.
# Once installed, later releases are one `noeta upgrade` away — this script is only needed once.
#
#   curl -fsSL https://raw.githubusercontent.com/noeta-lang/noeta/main/install.sh | sh
#
# Options (flags or environment):
#   --version vX.Y.Z   / NOETA_VERSION      install a specific release (default: latest)
#   --to <dir>         / NOETA_INSTALL_DIR  install directory (default: ~/.local/bin)
#
# Supported targets: x86_64/aarch64 Linux (gnu) and macOS. Anything else (musl, Windows,
# *BSD) builds from source instead: https://github.com/noeta-lang/noeta#building-from-source

set -eu

REPO="noeta-lang/noeta"
VERSION="${NOETA_VERSION:-}"
INSTALL_DIR="${NOETA_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() { printf 'noeta install: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --to) INSTALL_DIR="$2"; shift 2 ;;
    -h|--help) sed -n '2,13p' "$0" 2>/dev/null || true; exit 0 ;;
    *) fail "unknown option \`$1\` (try --version <tag>, --to <dir>)" ;;
  esac
done

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

# --- Detect the release target for this machine -------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
  Linux) OS_PART="unknown-linux-gnu" ;;
  Darwin) OS_PART="apple-darwin" ;;
  *) fail "unsupported OS \`$OS\` — build from source: https://github.com/$REPO#building-from-source" ;;
esac
case "$ARCH" in
  x86_64|amd64) ARCH_PART="x86_64" ;;
  aarch64|arm64) ARCH_PART="aarch64" ;;
  *) fail "unsupported architecture \`$ARCH\` — build from source: https://github.com/$REPO#building-from-source" ;;
esac
# A musl-only Linux (e.g. Alpine) cannot run the gnu build.
if [ "$OS_PART" = "unknown-linux-gnu" ] && [ ! -e /lib/ld-linux-x86-64.so.2 ] && [ ! -e /lib/ld-linux-aarch64.so.1 ]; then
  fail "this Linux lacks glibc (musl?) — build from source: https://github.com/$REPO#building-from-source"
fi
TARGET="$ARCH_PART-$OS_PART"

# --- Resolve the version ------------------------------------------------------------------------
if [ -z "$VERSION" ]; then
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
  [ -n "$VERSION" ] || fail "could not resolve the latest release (no releases yet, or GitHub API unreachable) — pass --version vX.Y.Z"
fi

# Prereleases are never installed — same definition as the release workflow: any `-` in the tag
# (v1.2.0-rc.1, v1.2.0-alpha). The latest-release API already excludes them; this also guards an
# explicit --version/NOETA_VERSION.
case "$VERSION" in
  *-*) fail "prerelease builds are not installable via install.sh (requested \`$VERSION\`) — only proper releases (vX.Y.Z) can be installed" ;;
esac

DIST="noeta-$VERSION-$TARGET"
BASE="https://github.com/$REPO/releases/download/$VERSION"

say "installing noeta $VERSION for $TARGET"

# --- Download + verify --------------------------------------------------------------------------
TMP=$(mktemp -d "${TMPDIR:-/tmp}/noeta-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

curl -fsSL -o "$TMP/$DIST.tar.gz" "$BASE/$DIST.tar.gz" \
  || fail "download failed: $BASE/$DIST.tar.gz (is $VERSION a released tag with binaries?)"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS" \
  || fail "download failed: $BASE/SHA256SUMS"

EXPECTED=$(sed -n "s/^\([0-9a-f]\{64\}\)[[:space:]]*\*\{0,1\}$DIST\.tar\.gz\$/\1/p" "$TMP/SHA256SUMS")
[ -n "$EXPECTED" ] || fail "no checksum for $DIST.tar.gz in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL=$(sha256sum "$TMP/$DIST.tar.gz" | cut -d' ' -f1)
else
  ACTUAL=$(shasum -a 256 "$TMP/$DIST.tar.gz" | cut -d' ' -f1)
fi
[ "$EXPECTED" = "$ACTUAL" ] || fail "checksum mismatch for $DIST.tar.gz (expected $EXPECTED, got $ACTUAL)"

# --- Install ------------------------------------------------------------------------------------
tar -xzf "$TMP/$DIST.tar.gz" -C "$TMP"
[ -f "$TMP/$DIST/noeta" ] || fail "unexpected archive layout: no $DIST/noeta inside the tarball"
mkdir -p "$INSTALL_DIR"
install -m 755 "$TMP/$DIST/noeta" "$INSTALL_DIR/noeta" 2>/dev/null \
  || { cp "$TMP/$DIST/noeta" "$INSTALL_DIR/noeta" && chmod 755 "$INSTALL_DIR/noeta"; }

say "installed $INSTALL_DIR/noeta"

# --- PATH guidance ------------------------------------------------------------------------------
case ":$PATH:" in
  *:"$INSTALL_DIR":*) "$INSTALL_DIR/noeta" --version 2>/dev/null || true ;;
  *)
    say ""
    say "$INSTALL_DIR is not on your PATH. Add it:"
    say "  bash/zsh:  export PATH=\"$INSTALL_DIR:\$PATH\"   (append to your shell profile)"
    say "  fish:      fish_add_path $INSTALL_DIR"
    ;;
esac
