#!/bin/bash
# Second attribution round. Two things the first round turned up need controls:
#   * `.len()` as the sink is not free — a1_str/w1_str came out ABOVE the full benchmark, so the
#     sink has to be priced separately (constant string, same sink) before the interpolation is.
#   * the inline-key spelling of wordcount cost 240x the named-key spelling. That is a cliff, not
#     a constant, so it gets its own scaling ladder to prove what it is.
# Same instrument: instructions retired, min of N, one CPU, each fixture alone in its own dir.
set -u
BIN=${1:?usage: attrib2.sh <noeta-binary> [label]}
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

# ---- sink control: same shape as a1_str, constant key, so `.len()` is priced on its own ---------
fixture s0_sink 'mut sum = 0
mut i = 0
while i < 100000 {
    k = "key"
    sum = sum + k.len()
    i = i + 1
}
echo sum'

fixture s1_interp 'mut sum = 0
mut i = 0
while i < 100000 {
    k = "key${i}"
    sum = sum + k.len()
    i = i + 1
}
echo sum'

# ---- the inline-key cliff, at three map sizes ---------------------------------------------------
# 20k iterations each so the slow spelling finishes. If the slow form is copying the map, its cost
# scales with the map's ENTRY COUNT while the fast form does not.
for n in 50 500; do
  fixture "c_named_$n" "mut m: Map<string, int> = {}
mut i = 0
while i < 20000 {
    key = \"word\${i % $n}\"
    m[key] = m.get_or(key, 0) + 1
    i = i + 1
}
echo m[\"word0\"]"
  fixture "c_inline_$n" "mut m: Map<string, int> = {}
mut i = 0
while i < 20000 {
    m[\"word\${i % $n}\"] = m.get_or(\"word\${i % $n}\", 0) + 1
    i = i + 1
}
echo m[\"word0\"]"
  # Which side triggers it: inline only on the LHS (subscript target), named on the RHS.
  fixture "c_lhs_$n" "mut m: Map<string, int> = {}
mut i = 0
while i < 20000 {
    key = \"word\${i % $n}\"
    m[\"word\${i % $n}\"] = m.get_or(key, 0) + 1
    i = i + 1
}
echo m[\"word0\"]"
  # ...and inline only on the RHS (the `get_or` argument).
  fixture "c_rhs_$n" "mut m: Map<string, int> = {}
mut i = 0
while i < 20000 {
    key = \"word\${i % $n}\"
    m[key] = m.get_or(\"word\${i % $n}\", 0) + 1
    i = i + 1
}
echo m[\"word0\"]"
  # No `get_or` at all, inline key: does a plain inline-key store cliff too?
  fixture "c_store_$n" "mut m: Map<string, int> = {}
mut i = 0
while i < 20000 {
    m[\"word\${i % $n}\"] = i
    i = i + 1
}
echo m[\"word0\"]"
done

echo "### attribution round 2 — $LABEL"
echo "# $BIN"
echo "# instructions retired, min of $REPS, pinned to CPU $CPU, each fixture alone in its own dir"
echo
for f in s0_sink s1_interp \
         c_named_50 c_inline_50 c_lhs_50 c_rhs_50 c_store_50 \
         c_named_500 c_inline_500 c_lhs_500 c_rhs_500 c_store_500; do
  out=$(taskset -c "$CPU" "$BIN" run "$ROOT/$f/prog.noe" 2>&1 | head -1)
  printf "  %-14s %14s   (prints %s)\n" "$f" "$(icount "$BIN" run "$ROOT/$f/prog.noe")" "$out"
done
