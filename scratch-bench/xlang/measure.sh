#!/bin/bash
# THE canonical perf measurement for this workstream. Every agent uses this — do not hand-roll
# another one, or the numbers stop being comparable across branches.
#
#   usage: measure.sh <path-to-noeta-binary> [label]
#
# Reports INSTRUCTIONS RETIRED, not wall-clock. This box regularly carries sibling agent builds
# (load 6-13 has been observed); under that load whole wall-clock field runs inflate 2x together,
# while instruction counts stay stable to ~0.03%. Instructions are also what a regression gate can
# assert on, so measuring them here is the same instrument the ratchet will use.
#
# Two traps this script exists to avoid:
#   * Sibling linking. The entry's siblings are linked as its project, so a fixture measured in a
#     directory with other .noe files silently costs O(directory) — that inflated a first-pass
#     "16x compile regression" claim into nonsense. Startup fixtures are therefore written into a
#     private temp dir, one file, every time.
#   * Locale + event naming. `perf stat -x,` emits the locale's decimal separator and suffixes the
#     event name (`instructions:u`), so parsing needs LC_ALL=C and a prefix match.
set -u
BIN=${1:?usage: measure.sh <noeta-binary> [label]}
LABEL=${2:-$(basename "$BIN")}
REPS=${MEASURE_REPS:-3}
CPU=${MEASURE_CPU:-3}
XL=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 1; }

SOLO=$(mktemp -d)
trap 'rm -rf "$SOLO"' EXIT
printf 'echo 0\n' > "$SOLO/empty.noe"

# min-of-REPS instructions retired for one command
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

echo "### $LABEL"
echo "# $BIN"
echo "# instructions retired, min of $REPS, pinned to CPU $CPU"
echo

# --- startup ladder: fixture ALONE in its own directory ---------------------------------------
echo "[startup, solo dir]"
printf "  %-16s %14s\n" "version"        "$(icount "$BIN" --version)"
printf "  %-16s %14s\n" "check"          "$(icount "$BIN" check "$SOLO/empty.noe")"
printf "  %-16s %14s\n" "run --no-cache" "$(icount "$BIN" run --no-cache "$SOLO/empty.noe")"
printf "  %-16s %14s\n" "run (cached)"   "$(icount "$BIN" run "$SOLO/empty.noe")"
echo

# --- the five workload benchmarks -------------------------------------------------------------
echo "[benchmarks]"
for b in loop fib strcat assoc wordcount; do
  printf "  %-16s %14s\n" "$b" "$(icount "$BIN" run "$XL/$b.noe")"
done
echo

# --- correctness: a faster wrong answer is not a result ----------------------------------------
echo "[output check]"
declare -A EXPECT=([loop]=29999994 [fib]=2178309 [strcat]=50000 \
                   [assoc]=4999950000 [wordcount]=400)
fail=0
for b in loop fib strcat assoc wordcount; do
  got=$(taskset -c "$CPU" "$BIN" run "$XL/$b.noe" 2>&1 | head -1)
  if [ "$got" = "${EXPECT[$b]}" ]; then printf "  %-16s ok\n" "$b"
  else printf "  %-16s MISMATCH got=%s want=%s\n" "$b" "$got" "${EXPECT[$b]}"; fail=1; fi
done
echo

# --- tiering: what tier 1 declined, and whether anything declined at all ------------------------
# `--jit-stats` names the ops that kept a hot loop off tier 1. Anything listed for assoc/wordcount/
# strcat is the actual thing standing between that benchmark and native code.
#
# This column must never be able to print blank. It used to grep for a fixed list of op names, so
# "nothing declined" and "the grep no longer matches the report" looked identical — an empty line
# either way, read as a pass over something never examined. `jitstats.py` states which of the two
# it is, and exits non-zero when the report shape it depends on has changed, which fails this
# script instead of quietly measuring nothing. `xrun3.py` reads the report through the same parser,
# so the two instruments cannot drift apart.
echo "[tier-1 decline]"
for b in loop assoc wordcount strcat; do
  line=$(python3 "$XL/jitstats.py" "$BIN" "$XL/$b.noe"); rc=$?
  printf "  %-10s %s\n" "$b" "$line"
  [ "$rc" -eq 0 ] || fail=1
done
echo
exit $fail
