#!/bin/bash
# Instruction-count ATTRIBUTION ladder for the map benchmarks (assoc, wordcount).
#
# Each fixture is written ALONE into its own temp directory (sibling linking is O(directory), see
# measure.sh) and measured as instructions retired, min of N, pinned to one CPU — the same
# instrument measure.sh and scripts/perf-ratchet.sh use. Subtracting adjacent rungs prices one
# ingredient of the benchmark at a time without touching engine code.
#
#   usage: attrib.sh <path-to-noeta-binary> [label]
set -u
BIN=${1:?usage: attrib.sh <noeta-binary> [label]}
LABEL=${2:-$(basename "$BIN")}
REPS=${MEASURE_REPS:-3}
CPU=${MEASURE_CPU:-3}

[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 1; }
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT

fixture() { # name, source
  mkdir -p "$ROOT/$1"
  printf '%s\n' "$2" > "$ROOT/$1/prog.noe"
}

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

# ---- the assoc ladder: 2 x 100k iterations, one ingredient added per rung -----------------------
fixture a0_ctl 'mut sum = 0
mut i = 0
while i < 100000 {
    i = i + 1
}
mut j = 0
while j < 100000 {
    sum = sum + j
    j = j + 1
}
echo sum'

fixture a1_str 'mut sum = 0
mut i = 0
while i < 100000 {
    k = "key${i}"
    sum = sum + k.len()
    i = i + 1
}
mut j = 0
while j < 100000 {
    k = "key${j}"
    sum = sum + k.len()
    j = j + 1
}
echo sum'

fixture a2_intmap 'mut m: Map<int, int> = {}
mut i = 0
while i < 100000 {
    m[i] = i
    i = i + 1
}
mut sum = 0
mut j = 0
while j < 100000 {
    sum = sum + m[j]
    j = j + 1
}
echo sum'

fixture a3_assoc 'mut m: Map<string, int> = {}
mut i = 0
while i < 100000 {
    m["key${i}"] = i
    i = i + 1
}
mut sum = 0
mut j = 0
while j < 100000 {
    sum = sum + m["key${j}"]
    j = j + 1
}
echo sum'

# ---- the wordcount ladder: 200k iterations -----------------------------------------------------
fixture w0_ctl 'mut s = 0
mut i = 0
while i < 200000 {
    s = s + (i % 500)
    i = i + 1
}
echo s'

fixture w1_str 'mut s = 0
mut i = 0
while i < 200000 {
    key = "word${i % 500}"
    s = s + key.len()
    i = i + 1
}
echo s'

fixture w2_wordcount 'mut m: Map<string, int> = {}
mut i = 0
while i < 200000 {
    key = "word${i % 500}"
    m[key] = m.get_or(key, 0) + 1
    i = i + 1
}
echo m["word0"]'

# Same algorithm, key spelled twice so the `set` sees a single-use temporary (`consume_key`)
# instead of a live named binding. Prices the owned-key clone the named form pays.
fixture w3_temp 'mut m: Map<string, int> = {}
mut i = 0
while i < 200000 {
    m["word${i % 500}"] = m.get_or("word${i % 500}", 0) + 1
    i = i + 1
}
echo m["word0"]'

echo "### attribution — $LABEL"
echo "# $BIN"
echo "# instructions retired, min of $REPS, pinned to CPU $CPU, each fixture alone in its own dir"
echo
for f in a0_ctl a1_str a2_intmap a3_assoc w0_ctl w1_str w2_wordcount w3_temp; do
  out=$(taskset -c "$CPU" "$BIN" run "$ROOT/$f/prog.noe" 2>&1 | head -1)
  printf "  %-14s %14s   (prints %s)\n" "$f" "$(icount "$BIN" run "$ROOT/$f/prog.noe")" "$out"
done
