#!/bin/bash
# Round 5: the blast radius of the map read-modify-write cliff.
#   * `m[k] = m[k] + 1` — an `Index` read on the right, not a `CallMethod`.
#   * the same shapes inside a function, where `m` is a register local rather than a global slot.
set -u
BIN=${1:?usage: attrib5.sh <noeta-binary> [label]}
LABEL=${2:-$(basename "$BIN")}
REPS=${MEASURE_REPS:-3}
CPU=${MEASURE_CPU:-3}
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 1; }
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
fixture() { mkdir -p "$ROOT/$1"; printf '%s\n' "$2" > "$ROOT/$1/prog.noe"; }
icount() {
  local best="" v
  for _ in $(seq "$REPS"); do
    v=$(LC_ALL=C perf stat -x, -e instructions:u taskset -c "$CPU" "$@" 2>&1 >/dev/null \
        | awk -F, '$3 ~ /^instructions/ {print $1; exit}')
    [ -n "${v:-}" ] || continue
    if [ -z "$best" ] || [ "$v" -lt "$best" ]; then best=$v; fi
  done
  echo "${best:-0}"
}

for n in 50 500; do
  # Index read on the right instead of a method call.
  fixture "idx_$n" "mut m: Map<int, int> = {}
mut p = 0
while p < $n {
    m[p] = 0
    p = p + 1
}
mut i = 0
while i < 20000 {
    k = i % $n
    m[k] = m[k] + 1
    i = i + 1
}
echo m[0]"
  # Same, inside a function: `m` is a register local, not a global slot.
  fixture "fn_getor_$n" "fn go(): int {
    mut m: Map<int, int> = {}
    mut p = 0
    while p < $n {
        m[p] = 0
        p = p + 1
    }
    mut i = 0
    while i < 20000 {
        k = i % $n
        m[k] = m.get_or(k, 0) + 1
        i = i + 1
    }
    return m[0]
}
echo go()"
  fixture "fn_inline_$n" "fn go(): int {
    mut m: Map<int, int> = {}
    mut p = 0
    while p < $n {
        m[p] = 0
        p = p + 1
    }
    mut i = 0
    while i < 20000 {
        m[i % $n] = m.get_or(i % $n, 0) + 1
        i = i + 1
    }
    return m[0]
}
echo go()"
done

echo "### attribution round 5 — $LABEL"
echo "# $BIN"
echo "# instructions retired, min of $REPS, pinned to CPU $CPU, each fixture alone in its own dir"
echo
for f in idx_50 fn_getor_50 fn_inline_50 idx_500 fn_getor_500 fn_inline_500; do
  out=$(taskset -c "$CPU" "$BIN" run "$ROOT/$f/prog.noe" 2>&1 | head -1)
  printf "  %-18s %14s   (prints %s)\n" "$f" "$(icount "$BIN" run "$ROOT/$f/prog.noe")" "$out"
done
