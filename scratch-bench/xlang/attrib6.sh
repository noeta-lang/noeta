#!/bin/bash
# Round 6: the standing open item "top-level list-index loops are +4.5% vs pre-arc, unattributed".
# Measures a top-level list-index loop (and the two reference rows) on today's binary against the
# pre-arc one, in the same session, in instructions retired.
set -u
REPS=${MEASURE_REPS:-3}
CPU=${MEASURE_CPU:-3}
BINDIR=${1:?usage: attrib6.sh <dir with noeta-jit, noeta-jit-0802, noeta-jit-prearc>}
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

fixture listidx 'mut xs: List<int> = []
mut p = 0
while p < 1000 {
    xs = xs ~ [p]
    p = p + 1
}
mut total = 0
mut i = 0
while i < 2000000 {
    total = total + xs[i % 1000]
    i = i + 1
}
echo total'

fixture listidx_str 'mut xs: List<string> = []
mut p = 0
while p < 1000 {
    xs = xs ~ ["v"]
    p = p + 1
}
mut total = 0
mut i = 0
while i < 500000 {
    total = total + xs[i % 1000].len()
    i = i + 1
}
echo total'

fixture toploop 'mut total = 0
mut i = 0
while i < 10000000 {
    total = total + (i % 7)
    i = i + 1
}
echo total'

echo "### attribution round 6 — top-level list indexing"
echo "# instructions retired, min of $REPS, pinned to CPU $CPU, each fixture alone in its own dir"
echo
printf "  %-14s %16s %16s %16s\n" fixture main-57091de12 "0802-7e7d038db" "pre-arc-95f14eeef"
for f in listidx listidx_str toploop; do
  a=$(icount "$BINDIR/noeta-jit" run "$ROOT/$f/prog.noe")
  b=$(icount "$BINDIR/noeta-jit-0802" run "$ROOT/$f/prog.noe")
  c=$(icount "$BINDIR/noeta-jit-prearc" run "$ROOT/$f/prog.noe")
  printf "  %-14s %16s %16s %16s\n" "$f" "$a" "$b" "$c"
done
