#!/usr/bin/env bash
# Point the para packages' Rust manifests at a toolchain release.
#
# WHY THIS EXISTS. Each para repo used to carry a `toolchain-pin.yml` that did this on a
# `release-published` dispatch and opened a PR. Opening the PR needs "Allow GitHub Actions to
# create and approve pull requests", which is off for this org, so the step pushed a branch nobody
# merged: eight stale `toolchain-pin/*` branches per repo, and a `main` still pinned to the
# previous release. The verification half was duplicated by each repo's own `ci.yml`, which reads
# `NOETA_VERSION` and runs on push, and the drift alarm lives there too as the consistency guard.
# So the workflow was retired and the one part that had no other home moved here, where the tag is
# already known and a human is already running the release.
#
#   scripts/pin-para.sh v0.8.0                 rewrite, then print what changed
#   scripts/pin-para.sh v0.8.0 --check         report drift, change nothing (exit 1 if any)
#   scripts/pin-para.sh v0.8.0 --dir <path>    where the para checkouts live (default: ../para)
#
# WHAT IT REWRITES, and why each rule is separate:
#
#   (a) git tag pins on the toolchain repo — the UNPUBLISHED internal crates (`noeta-loader`,
#       `noeta-stdlib`, `noeta-check`, ...). They carry no stability promise and must move in
#       lockstep with the toolchain.
#
#   (b) crates.io ranges for the contract crates — `noeta-ext-abi = "0.7"`. A patch release needs
#       no edit, which is the point of publishing them; a MINOR release does, because "0.7" does
#       not admit 0.8.0.
#
#   (c) an EXACT pin inside a `[patch]` table — `noeta-ext-abi = "=0.7.1"`. A `[patch]` entry has
#       to resolve to exactly one candidate, so it names a full version rather than a range, and it
#       has to track the tag that (a) just moved. Rule (b) cannot cover it: the value starts with
#       `=`, not a digit. Missing this is what broke para-api against v0.8.0 — the git source folded
#       onto a different copy of the contract crate than the published dependency, two `Extension`
#       traits existed, and the extension type stopped matching its own registry.
#
# EXIT CODES
#   0  every checkout is already current, or was rewritten successfully
#   1  --check found drift, or a rewrite failed
#   2  the para directory or a checkout is missing (cannot answer, not "nothing to do")
set -uo pipefail

TAG=""; MODE="write"; DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --check) MODE="check"; shift ;;
    --dir)   DIR="${2:-}"; shift 2 ;;
    v[0-9]*.[0-9]*.[0-9]*) TAG="$1"; shift ;;
    *) echo "pin-para: unrecognized argument '$1'" >&2; exit 2 ;;
  esac
done
[ -n "$TAG" ] || { echo "pin-para: needs a release tag, e.g. scripts/pin-para.sh v0.8.0" >&2; exit 2; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIR="${DIR:-$(cd "$HERE/../para" 2>/dev/null && pwd)}"
[ -n "$DIR" ] && [ -d "$DIR" ] || { echo "pin-para: no para checkouts at '${DIR:-../para}'" >&2; exit 2; }

MINOR="$(printf '%s' "${TAG#v}" | cut -d. -f1,2)"
FULL="${TAG#v}"
rc=0; changed=0; clean=0

for repo in "$DIR"/para-*/; do
  [ -d "${repo}.git" ] || continue
  name="$(basename "$repo")"
  # Only manifests that actually name the toolchain. A package with no Rust half has none.
  mapfile -t files < <(grep -rlE '^noeta-(ext|reactive)-abi = "=?[0-9]|github\.com/noeta-lang/noeta", tag = "' --include=Cargo.toml "$repo" 2>/dev/null)
  if [ "${#files[@]}" -eq 0 ]; then
    printf '  %-18s no toolchain pins\n' "$name"
    continue
  fi

  before="$(cat "${files[@]}")"
  tmp="$(mktemp -d)"
  for f in "${files[@]}"; do cp "$f" "$tmp/$(echo "$f" | tr '/' '_')"; done

  for f in "${files[@]}"; do
    sed -i -E 's|(github\.com/noeta-lang/noeta", tag = ")v[0-9][^"]*(")|\1'"$TAG"'\2|g' "$f"
    sed -i -E '/^noeta-(ext|reactive)-abi = "/ s|"[0-9]+\.[0-9]+"|"'"$MINOR"'"|' "$f"
    sed -i -E '/^noeta-(ext|reactive)-abi = "=/ s|"=[0-9]+\.[0-9]+\.[0-9]+"|"='"$FULL"'"|' "$f"
  done
  after="$(cat "${files[@]}")"

  if [ "$before" = "$after" ]; then
    printf '  %-18s already at %s\n' "$name" "$TAG"
    clean=$((clean + 1))
    rm -rf "$tmp"
    continue
  fi

  changed=$((changed + 1))
  if [ "$MODE" = "check" ]; then
    printf '  %-18s DRIFT — pinned below %s\n' "$name" "$TAG"
    for f in "${files[@]}"; do cp "$tmp/$(echo "$f" | tr '/' '_')" "$f"; done
    rc=1
  else
    printf '  %-18s rewritten to %s\n' "$name" "$TAG"
    # The diff is the evidence, so print it rather than asserting success.
    (cd "$repo" && git --no-pager diff --stat -- '*Cargo.toml' | sed 's/^/      /')
  fi
  rm -rf "$tmp"
done

echo
if [ "$MODE" = "check" ]; then
  [ "$rc" -eq 0 ] && echo "pin-para: every checkout is at $TAG" || echo "pin-para: $changed checkout(s) need the rewrite"
else
  echo "pin-para: $changed rewritten, $clean already current"
  echo "Commit and push each rewritten repo; its own CI builds against $TAG on the push."
fi
exit $rc
