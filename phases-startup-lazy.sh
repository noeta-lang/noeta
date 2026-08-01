#!/bin/bash
# Phase ladder for perf/startup-lazy: instructions retired up to each NOETA_PHASE_STOP point.
set -u
BIN=${1:?bin}
CPU=3
SOLO=$(mktemp -d); trap 'rm -rf "$SOLO"' EXIT
printf 'echo 0\n' > "$SOLO/empty.noe"
icount() {
  local best="" v
  for _ in 1 2 3; do
    v=$(LC_ALL=C perf stat -x, -e instructions:u taskset -c "$CPU" "$@" 2>&1 >/dev/null \
        | awk -F, '$3 ~ /^instructions/ {print $1; exit}')
    [ -n "${v:-}" ] || continue
    if [ -z "$best" ] || [ "$v" -lt "$best" ]; then best=$v; fi
  done
  echo "${best:-0}"
}
prev=0
row() { # label envval
  local v; v=$(NOETA_PHASE_STOP="$2" icount "$BIN" run --no-cache "$SOLO/empty.noe")
  printf "  %-18s %12s  (+%s)\n" "$1" "$v" "$((v-prev))"; prev=$v
}
echo "### phase ladder: $BIN"
v=$(icount "$BIN" --version); printf "  %-18s %12s\n" "--version" "$v"; prev=$v
row enter            enter
row front            front
row cachekey         cachekey
row load             load
row "  checker-new"  checker-new
row "  prelude-builtin" prelude-builtin
row "  prelude-attrs"   prelude-attrs
row "  prelude-traits"  prelude-traits
row "  prelude-enums"   prelude-enums
row "  prelude-fielded" prelude-fielded
row "  prelude(all)"    prelude
row "  collect"         collect
row check            check
row "  install-enter"   install-enter
row "  hoist"           hoist
row "  lower"           lower
row "  facts"           facts
row "  passes"          passes
row "  globals"         globals
row "  regtypes"        regtypes
row "  methods"         methods
row "  packed"          packed
row "  roles"           roles
row "  nativetraits"    nativetraits
row "  reflectbuild"    reflectbuild
row "  extendreflect"   extendreflect
row compile          compile
row host             host
row vm               vm
v=$(icount "$BIN" run --no-cache "$SOLO/empty.noe"); printf "  %-18s %12s  (+%s)\n" "full" "$v" "$((v-prev))"
