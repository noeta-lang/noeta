#!/bin/bash
# Round 4: does the stale-register aliasing cliff reach LISTS too? Same read-modify-write shape,
# `l[k] = l[k] + 1`, at two list lengths. Cost growing with the list's LENGTH means the list is
# copied per iteration.
set -u
BIN=${1:?usage: attrib4.sh <noeta-binary> [label]}
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
  fixture "l_rmw_$n" "mut l: List<int> = []
mut p = 0
while p < $n {
    l = l ~ [0]
    p = p + 1
}
mut i = 0
while i < 20000 {
    l[i % $n] = l[i % $n] + 1
    i = i + 1
}
echo l[0]"
  fixture "l_named_$n" "mut l: List<int> = []
mut p = 0
while p < $n {
    l = l ~ [0]
    p = p + 1
}
mut i = 0
while i < 20000 {
    x = i % $n
    v = l[x]
    l[x] = v + 1
    i = i + 1
}
echo l[0]"
done

echo "### attribution round 4 — $LABEL"
echo "# $BIN"
echo "# instructions retired, min of $REPS, pinned to CPU $CPU, each fixture alone in its own dir"
echo
for f in l_rmw_50 l_named_50 l_rmw_500 l_named_500; do
  out=$(taskset -c "$CPU" "$BIN" run "$ROOT/$f/prog.noe" 2>&1 | head -1)
  printf "  %-18s %14s   (prints %s)\n" "$f" "$(icount "$BIN" run "$ROOT/$f/prog.noe")" "$out"
done
