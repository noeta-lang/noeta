#!/bin/bash
# Round 3: how far does the stale-register aliasing cliff reach? The map form
# `m[k] = m.get_or(k, 0) + 1` copies the whole map per iteration when a dead register still holds
# a reference to it. Lists and strings take the same `refcount() != 1 -> copy` branch, so they get
# the same scaling test: if the cost grows with the container's LENGTH, the container is being
# copied per iteration.
set -u
BIN=${1:?usage: attrib3.sh <noeta-binary> [label]}
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
    l.push(0)
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
    l.push(0)
    p = p + 1
}
mut i = 0
while i < 20000 {
    x = i % $n
    l[x] = l[x] + 1
    i = i + 1
}
echo l[0]"
  fixture "m_rmw_$n" "mut m: Map<int, int> = {}
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
echo m[0]"
  fixture "m_rmw_inline_$n" "mut m: Map<int, int> = {}
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
echo m[0]"
done

# Strings take the same branch: `s = s ~ x` extends in place only while sole-owned.
fixture "str_plain" 'mut s = ""
mut i = 0
while i < 20000 {
    s = s ~ "x"
    i = i + 1
}
echo s.len()'
fixture "str_alias" 'mut s = ""
mut i = 0
while i < 20000 {
    t = s
    s = s ~ "x"
    n = t.len()
    i = i + 1
}
echo s.len()'

echo "### attribution round 3 — $LABEL"
echo "# $BIN"
echo "# instructions retired, min of $REPS, pinned to CPU $CPU, each fixture alone in its own dir"
echo
for f in l_rmw_50 l_named_50 m_rmw_50 m_rmw_inline_50 \
         l_rmw_500 l_named_500 m_rmw_500 m_rmw_inline_500 str_plain str_alias; do
  out=$(taskset -c "$CPU" "$BIN" run "$ROOT/$f/prog.noe" 2>&1 | head -1)
  printf "  %-18s %14s   (prints %s)\n" "$f" "$(icount "$BIN" run "$ROOT/$f/prog.noe")" "$out"
done
