#!/usr/bin/env bash
# The registry-protocol fixture sync — one command in place of a four-step ritual.
#
# `crates/noeta-pm/test_data/wire/` is the canonical wire-fixture set; the hosted registry
# (a SEPARATE repo, `noeta-registry`) carries a verbatim copy at `test/fixtures/wire/`. Two repos
# cannot share a file, so the copy is pinned by `MANIFEST.sha256` — and the manifest is pinned, in
# turn, by a SOURCE constant on each side that is deliberately *not* inside the copied directory:
#
#   lang      crates/noeta-pm/src/registry.rs   pub const WIRE_MANIFEST_SHA256
#   registry  src/wire-manifest.ts              export const WIRE_MANIFEST_SHA256
#
# Without that stamp the pin is self-referential: each repo hashes its own fixtures against its own
# manifest, so "regenerated here, never copied there" is green on both sides while the protocol
# diverges (audit row 4, item 3). With it, the receiving repo fails by name.
#
# This script is the chokepoint. It regenerates the manifest, rewrites BOTH stamps, and copies the
# set across — so the step everyone could forget is inside a single command rather than step 3 of a
# README. Run it after editing any fixture, then run both suites:
#
#   scripts/sync-wire-fixtures.sh
#   cargo test -p noeta-pm --all-features        # here
#   (cd ../noeta-registry && pnpm test)          # there
#
# `--check` is the read-only assertion (no writes, non-zero on drift) that CI and `scripts/gate.sh`
# run. It verifies this repo's stamp against this repo's manifest, and — when the registry checkout
# is present — that the copy and its stamp are identical too.
#
# The registry checkout is found via $NOETA_REGISTRY_DIR, else `../noeta-registry` beside the main
# worktree. In `--check` it is optional (CI has one repo); in sync mode its absence is a hard error,
# because a sync that silently updated only half of the pair is the failure this exists to end.

set -euo pipefail

CHECK=0
case "${1:-}" in
    --check) CHECK=1 ;;
    "") ;;
    *)
        echo "usage: $0 [--check]" >&2
        exit 2
        ;;
esac

# Fixtures and the Rust stamp come from THIS worktree — agents work in `.claude/worktrees/<name>`,
# and a script that reached into the shared checkout would rewrite files nobody asked it to. The
# main worktree's location is used only to guess where the sibling registry clone lives.
REPO_ROOT=$(git rev-parse --show-toplevel)
MAIN_ROOT=$(cd "$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")" && pwd)
WIRE_DIR="$REPO_ROOT/crates/noeta-pm/test_data/wire"
RUST_STAMP_FILE="$REPO_ROOT/crates/noeta-pm/src/registry.rs"
TS_STAMP_REL="src/wire-manifest.ts"

REGISTRY_DIR="${NOETA_REGISTRY_DIR:-$(dirname "$MAIN_ROOT")/noeta-registry}"

fail() {
    echo "sync-wire-fixtures: $*" >&2
    exit 1
}

[ -d "$WIRE_DIR" ] || fail "no fixture directory at $WIRE_DIR"

# --- 1. the manifest ------------------------------------------------------------------------------

if [ "$CHECK" -eq 0 ]; then
    (cd "$WIRE_DIR" && sha256sum ./*.json | sed 's|\./||' > MANIFEST.sha256)
fi
[ -f "$WIRE_DIR/MANIFEST.sha256" ] || fail "no MANIFEST.sha256 in $WIRE_DIR"
STAMP=$(sha256sum "$WIRE_DIR/MANIFEST.sha256" | cut -d' ' -f1)

# --- 2. the two source stamps ---------------------------------------------------------------------

# Anchored on the declaration, not on "a line that looks like a hash": rustfmt puts the value on the
# next line, so both reader and writer match across the newline.
read_rust_stamp() {
    perl -0ne 'print $1 if /pub const WIRE_MANIFEST_SHA256: &str =\s*"([0-9a-f]{64})";/' \
        "$RUST_STAMP_FILE"
}
read_ts_stamp() { sed -n 's/^export const WIRE_MANIFEST_SHA256 = "\([0-9a-f]\{64\}\)";$/\1/p' "$1"; }

write_rust_stamp() {
    # The constant is written across two lines by rustfmt; the hash is alone on the second.
    perl -0pi -e "s/(pub const WIRE_MANIFEST_SHA256: &str =\s*\n?\s*\")[0-9a-f]{64}(\";)/\${1}$STAMP\${2}/" \
        "$RUST_STAMP_FILE"
}
write_ts_stamp() {
    perl -pi -e "s/^(export const WIRE_MANIFEST_SHA256 = \")[0-9a-f]{64}(\";)$/\${1}$STAMP\${2}/" "$1"
}

if [ "$CHECK" -eq 0 ]; then
    write_rust_stamp
fi
HAVE=$(read_rust_stamp)
[ -n "$HAVE" ] || fail "cannot find WIRE_MANIFEST_SHA256 in $RUST_STAMP_FILE"
if [ "$HAVE" != "$STAMP" ]; then
    fail "the lang stamp is stale: WIRE_MANIFEST_SHA256 is $HAVE, the fixtures hash to $STAMP.
  The wire fixtures changed without the protocol stamp moving. Run scripts/sync-wire-fixtures.sh."
fi

# --- 3. the registry copy -------------------------------------------------------------------------

if [ ! -d "$REGISTRY_DIR" ]; then
    if [ "$CHECK" -eq 1 ]; then
        echo "sync-wire-fixtures: no registry checkout at $REGISTRY_DIR — checked this repo only."
        echo "  (set NOETA_REGISTRY_DIR to check the copy and its stamp as well)"
        exit 0
    fi
    fail "no registry checkout at $REGISTRY_DIR.
  Set NOETA_REGISTRY_DIR, or clone it beside this repo. Syncing only one side of a two-repo
  protocol is exactly the failure this script exists to end, so this is an error, not a warning."
fi

DEST="$REGISTRY_DIR/test/fixtures/wire"
TS_STAMP_FILE="$REGISTRY_DIR/$TS_STAMP_REL"
[ -d "$DEST" ] || fail "no fixture directory at $DEST"
[ -f "$TS_STAMP_FILE" ] || fail "no stamp file at $TS_STAMP_FILE"

if [ "$CHECK" -eq 0 ]; then
    # Mirror, not merge: a fixture deleted here must disappear there, or the registry keeps testing a
    # shape the protocol no longer has.
    rm -f "$DEST"/*.json "$DEST/MANIFEST.sha256" "$DEST/README.md"
    cp "$WIRE_DIR"/*.json "$WIRE_DIR/MANIFEST.sha256" "$WIRE_DIR/README.md" "$DEST/"
    write_ts_stamp "$TS_STAMP_FILE"
fi

if ! diff -r -q "$WIRE_DIR" "$DEST" > /dev/null; then
    echo "sync-wire-fixtures: the registry's fixture copy differs from the canonical set:" >&2
    diff -r -q "$WIRE_DIR" "$DEST" >&2 || true
    fail "run scripts/sync-wire-fixtures.sh (without --check) to re-copy"
fi

TS_HAVE=$(read_ts_stamp "$TS_STAMP_FILE")
[ -n "$TS_HAVE" ] || fail "cannot find WIRE_MANIFEST_SHA256 in $TS_STAMP_FILE"
if [ "$TS_HAVE" != "$STAMP" ]; then
    fail "the registry stamp is stale: $TS_STAMP_REL says $TS_HAVE, the fixtures hash to $STAMP.
  Run scripts/sync-wire-fixtures.sh."
fi

if [ "$CHECK" -eq 1 ]; then
    echo "sync-wire-fixtures: in sync ($STAMP)"
else
    echo "sync-wire-fixtures: synced $(ls "$WIRE_DIR"/*.json | wc -l) fixtures + MANIFEST.sha256"
    echo "  stamp     $STAMP"
    echo "  lang      crates/noeta-pm/src/registry.rs"
    echo "  registry  $REGISTRY_DIR/$TS_STAMP_REL"
    echo "  Both repos now need a commit — the registry one is NOT in this repo's git status."
fi
