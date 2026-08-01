#!/usr/bin/env bash
# The performance ratchet — a structural guard on INSTRUCTIONS RETIRED, in the same spirit as
# `crates/noeta-bytecode/tests/op_size.rs` pinning `size_of::<Op>() <= 64`.
#
# WHY THIS EXISTS. Roughly 1,800 commits landed a 2x startup regression and a 7-11% interpreter
# regression and nothing in the tree noticed. Everything that could have noticed was wall-clock,
# and wall-clock cannot gate on a box that carries several concurrent agent builds: load 6-13 is
# normal here and a whole field run of wall-clock benchmarks inflates ~2x together, so a 7%
# regression is indistinguishable from a busy afternoon. Instructions retired is a different
# instrument: for the rows pinned below it repeats to 0.001-0.08% across runs on the SAME load
# that moves wall-clock by 2x (see MEASURED VARIANCE). That is deterministic enough to assert on,
# so this asserts on it.
#
#   scripts/perf-ratchet.sh              measure and compare against tests/perf/baseline.txt
#   scripts/perf-ratchet.sh --record     re-record the baseline from this build (see RE-BASELINING)
#   scripts/perf-ratchet.sh --preflight  exit 0 iff this machine can measure at all (gate.sh uses it)
#   scripts/perf-ratchet.sh --list       print the rows and their tolerances
#
# EXIT CODES — the whole point is that "could not measure" is not "passed".
#   0  every row is inside its tolerance band, and every tier-1 expectation held.
#   1  a row moved outside its band, or a tier-1 expectation broke. Read the report.
#   2  COULD NOT MEASURE: no perf, perf_event_paranoid forbids counting, no PMU exposed, no
#      binary, a debug binary, no baseline, a baseline missing a row, or a baseline recorded on a
#      different cpu / libc / rustc. Never conflated with 0. A predecessor
#      gate in this repo ("fix(bench): a regression gate that measured nothing must not report
#      success", c619853bd) shipped exactly that conflation; this file inherits its rule.
#
# THE TWO MEASUREMENT TRAPS, encoded here so nobody re-derives them (they come from
# scratch-bench/xlang/measure.sh, the canonical harness this shares its method with):
#
#   * SIBLING LINKING. An entry's siblings are linked as its project, so a fixture measured in a
#     populated directory silently costs O(directory) — that once inflated a startup number by 7x
#     and invalidated a whole analysis. Every fixture here is therefore COPIED, alone, into its
#     own directory under a private temp dir before it is measured. The in-repo layout
#     (tests/perf/fixtures/<row>/<row>.noe) mirrors that: one fixture per directory.
#   * LOCALE + EVENT NAMING. `perf stat -x,` emits the locale's decimal separator and suffixes the
#     event name (`instructions:u`), so parsing needs `LC_ALL=C` and a PREFIX match on the name.
#
# ...and a third this file found on top of those:
#
#   * ABSOLUTE PATH LENGTH IS PART OF THE MEASUREMENT. Measured: the same `run --no-cache` fixture
#     costs 5.546 M instructions at `/tmp/pg9/empty.noe` and 5.582 M (+0.65%) at a path 128 chars
#     longer — ~280 instructions per character of path. A baseline recorded in the shared checkout
#     and re-run from `.claude/worktrees/<name>` would drift ~0.2% for that reason alone. Copying
#     into `mktemp -d` fixes it: `/tmp/tmp.XXXXXXXXXX/<row>/<row>.noe` has a CONSTANT length
#     wherever the repo lives.
#
# WHAT IS PINNED, and why each row earns its place:
#
#   version    `noeta --version` — process init only. The 2x startup regression lived here.
#   startup    `run --no-cache` on a one-line program, alone in its directory — init + the whole
#              front end (lex/parse/check/lower/compile) + a trivial run.
#   arith      an interpreter-bound arithmetic loop. Tier 1 DECLINES it by construction (the loop
#              body carries a `CallMethod`, which is not a native op), so this row measures the
#              tier-0 dispatch loop — which is what regressed 7-11%.
#   map        an interpreter-bound map insert+lookup loop. Same construction; tier 1 declines on
#              `Stringify`/`BuildString`/`CallMethod`.
#   loop_jit   the same arithmetic under TIER 1. Its number is deliberately banded loosely — see
#              below — but its tier-1 expectation is `native`, and THAT is the sharp instrument on
#              this row (see TIER-1 DECLINE).
#
# Every row runs the whole binary, so every row carries process init and a compile (`--no-cache`
# on purpose: a warm bytecode cache is state, and a cold-vs-warm cache moved one of these numbers
# 5%). READ THE ROWS TOGETHER — that layering is what localizes a regression rather than merely
# detecting it. All five rows up by a similar absolute amount is process init; `startup` up while
# `version` holds is the front end; `arith` and `map` up while the first two hold is the
# interpreter; `loop_jit` alone is the JIT.
#
# MEASURED VARIANCE and the tolerances that follow from it. All measured on this box with the
# v0.4.0 release binary, `taskset -c 3`, while sibling agent builds held load between 6 and 29.
# Numbers are (max-min)/min over consecutive raw repetitions:
#
#   row        raw spread   samples   tolerance   headroom vs. the 7% we must catch
#   version      0.055%       12        1.0%      18x the noise, 7x below the target
#   startup      0.077%       12        1.0%      13x the noise, 7x below the target
#   arith        0.0005%       8        1.0%      2000x the noise
#   map          0.001%        6        1.0%      1000x the noise
#   loop_jit     2.06%        30        6.0%      see below
#
#   1.0% is not a guess and not the tightest number that would work; it is ~13x the worst observed
#   spread on a deterministic row, which buys room for the second-order effects that are real but
#   small: the size of the caller's environment block (+8 KB of env moved `startup` by 0.04%) and
#   a differing TMPDIR. It still fires on anything a quarter the size of the regression that
#   prompted this file.
#
#   loop_jit is the exception, and it is honest about it. Tier 1 compiles the loop on a BACKGROUND
#   thread, so the number of interpreted iterations that run before native code is installed
#   depends on how quickly that thread gets scheduled — i.e. on machine load. Measured: min-of-5
#   at load ~6 was 1.170 G instructions and at load ~25 was 1.184 G, a 1.2% shift in the FLOOR,
#   with a further 0.45% of spread around it; unpinned it was worse (4.6% raw spread). No
#   estimator fixes a moving floor, so this row gets a 6% band and is documented as coarse. It
#   still catches a 2x-class regression, and the precise guard on this row is not the number:
#   it is `tier1=native`.
#
# TIER-1 DECLINE — the check that three benchmark reports missed. If a benchmark's JIT column and
# its interpreter column are EQUAL, the JIT is not running on that benchmark at all, and that is
# the finding, not a footnote. `noeta run --jit-stats` names the ops that declined tier 1. Every
# row here declares what it expects (`native`, `declined`, or `n/a`) and the run FAILS if the
# answer changed — in either direction. A row that starts compiling natively is good news and
# still fails, because its pinned number is now measuring something else.
#
# RE-BASELINING. Expect these numbers to move: perf work lands here continuously.
#
#     scripts/perf-ratchet.sh --record
#
# re-measures every row from the current build and rewrites tests/perf/baseline.txt, which is a
# plain, diff-friendly text file — the diff in review IS the performance change, in instructions,
# with a percentage beside it. Read the sign before you commit it:
#
#   * numbers went DOWN  -> an improvement. Record it. The ratchet now holds the new floor;
#                          that is the entire point of a ratchet and why an improvement past the
#                          band is a FAIL by default rather than a silent pass.
#   * numbers went UP    -> a regression, unless you can say which change bought it and what for.
#                          "The gate was annoying" is not that reason. If it is a deliberate
#                          trade (a feature that costs startup), record it and say so in the
#                          commit message — the baseline diff is the record.
#
# Environment:
#   NOETA_PERF_BIN      the binary to measure. Default: $CARGO_TARGET_DIR/release/noeta (or
#                       target/release/noeta). It must be a RELEASE build: the baseline is
#                       recorded from one, and a debug binary is a different program by an order
#                       of magnitude. The gate checks and refuses.
#   NOETA_PERF_REPS     repetitions per row; the reported figure is the MINIMUM (default 5). The
#                       min is the right estimator here: every source of variance listed above
#                       ADDS work (a slower JIT-install race, a preempted run), so the floor is
#                       the signal and the tail is noise.
#   NOETA_PERF_CPU      CPU to pin to with taskset (default 3). Pinning matters: unpinned,
#                       loop_jit's raw spread went from 2.1% to 4.6%.
#   NOETA_PERF_ALLOW_IMPROVEMENT=1
#                       downgrade "faster than baseline, past the band" from FAIL to a warning.
#                       Off by default on purpose — see RE-BASELINING.
#   NOETA_PERF_TOL_SCALE
#                       multiply every tolerance (e.g. 2 to double them). For a machine you know
#                       is noisier than this one. Widening the band is a decision; make it
#                       explicitly and in the open, not by deleting a row.

set -uo pipefail

# `LC_ALL=C` for the WHOLE script, not just around perf. Trap 2 in the header is about parsing
# perf's output, but the same locale bites the arithmetic: under a comma-decimal locale, `awk`
# hands back "1.40" and bash's `printf %.2f` rejects it as an invalid number, so every delta this
# script computed printed as `+0,00%` — a gate reporting "no change" on a row that had moved 1.4%.
# Found by running it. Setting it once here closes the class instead of the instance, and has the
# side benefit of making the measured process's environment identical for every caller, whatever
# locale they run under.
export LC_ALL=C

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURES="$ROOT/tests/perf/fixtures"
BASELINE="$ROOT/tests/perf/baseline.txt"

REPS="${NOETA_PERF_REPS:-5}"
CPU="${NOETA_PERF_CPU:-3}"
TOL_SCALE="${NOETA_PERF_TOL_SCALE:-1}"
ALLOW_IMPROVEMENT="${NOETA_PERF_ALLOW_IMPROVEMENT:-0}"

MODE=compare

usage() { sed -n '2,/^set -uo/p' "${BASH_SOURCE[0]}" | sed -e 's/^# \{0,1\}//' -e '$d'; }

while (($#)); do
    case "$1" in
        --record) MODE=record ;;
        --preflight) MODE=preflight ;;
        --list) MODE=list ;;
        --bin)
            NOETA_PERF_BIN="${2:?--bin needs a path}"
            shift
            ;;
        --reps)
            REPS="${2:?--reps needs a number}"
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "perf-ratchet: unknown option '$1' (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

# ------------------------------------------------------------------------------------- the rows
#
# One row per line: id | tolerance % | tier-1 expectation | expected stdout | argv after the binary
# ("@" is replaced by the fixture path; a row with no fixture just omits it).
#
# This array is the single source of truth for what is measured. `tests/perf/baseline.txt` carries
# only the numbers, and `crates/noeta-cli/tests/perf_ratchet.rs` fails the build if the two ever
# disagree — a row that loses its fixture, or a fixture nothing measures, is how a gate quietly
# stops gating.
ROWS=(
    "version|1.0|n/a||--version"
    "startup|1.0|n/a|0|run --no-cache @"
    "arith|1.0|declined|1199997|run --no-cache @"
    "map|1.0|declined|449985000|run --no-cache @"
    "loop_jit|6.0|native|29999994|run --no-cache @"
)

row_field() { # <row-string> <1-based field>
    printf '%s' "$1" | cut -d'|' -f"$2"
}

if [[ "$MODE" == list ]]; then
    printf '%-10s %8s  %-9s %s\n' row tolerance tier-1 command
    for r in "${ROWS[@]}"; do
        printf '%-10s %7s%%  %-9s noeta %s\n' \
            "$(row_field "$r" 1)" "$(row_field "$r" 2)" "$(row_field "$r" 3)" "$(row_field "$r" 5)"
    done
    exit 0
fi

# -------------------------------------------------------------------------------------- preflight
#
# Everything that has to be true before a number this script prints means anything. Each failure
# names the exact missing piece and the command that fixes it, and every one of them exits 2 —
# "could not measure" — never 0.

cannot() { # <reason> [fix]
    echo "perf-ratchet: CANNOT MEASURE — $1" >&2
    [[ -n "${2:-}" ]] && echo "perf-ratchet: fix: $2" >&2
    exit 2
}

BIN="${NOETA_PERF_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/release/noeta}"

# The machine fingerprint. Instruction counts are portable across runs, not across machines: a
# different microarchitecture retires a different number of instructions for the same work, and a
# different libc changes process init outright. Comparing this box's numbers against another box's
# baseline is not a regression signal, it is a category error — so the baseline records where it
# was taken and a mismatch is exit 2 (could not measure), never a failure and never a pass.
#
# The compiler is part of the fingerprint for the same reason. `rustc 1.97.0` and `rustc 1.97.1`
# inline differently, and this repo's most recent perf commit — "set_reg inlines again — the
# dispatch loop outgrew LLVM's budget" — is precisely a case where an inlining decision moved the
# interpreter by percent. gate.sh gates with the CI pin (`+1.97.0`) and passes it in as
# NOETA_PERF_TOOLCHAIN; a baseline recorded from a binary built by a different rustc is a
# different program, so it stops the run instead of reporting a regression nobody wrote.
fingerprint() {
    local cpu libc rs tc="${NOETA_PERF_TOOLCHAIN:-}"
    cpu="$(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo 2> /dev/null)"
    [[ -n "$cpu" ]] || cpu="unknown-cpu"
    libc="$(ldd --version 2> /dev/null | head -1)"
    [[ -n "$libc" ]] || libc="unknown-libc"
    if [[ -n "$tc" && "$tc" != default ]]; then
        rs="$(rustc "+$tc" -V 2> /dev/null)"
    else
        rs="$(rustc -V 2> /dev/null)"
    fi
    [[ -n "$rs" ]] || rs="unknown-rustc"
    printf '%s | %s | %s' "$cpu" "$libc" "$rs"
}

baseline_fingerprint() {
    [[ -f "$BASELINE" ]] || return 1
    sed -n 's/^machine[[:space:]]*//p' "$BASELINE" | head -1
}

# Can this MACHINE produce a comparable number at all? Split from the checks below because the two
# have opposite meanings for the caller: this half answers "should we even try" (gate.sh turns a
# no into a visible SKIP), while a missing binary, a missing fixture or a missing baseline is a
# broken tree and must stay a hard failure nobody can mistake for a skip.
machine_preflight() {
    command -v perf > /dev/null 2>&1 \
        || cannot "\`perf\` is not on PATH (this ratchet counts instructions retired; there is no wall-clock fallback, because wall-clock cannot gate on a loaded box)" \
            "install linux-tools/perf for your kernel, e.g. \`sudo pacman -S perf\` or \`sudo apt install linux-tools-\$(uname -r)\`"

    # perf existing is not perf working. `perf_event_paranoid > 2`, a container without
    # CAP_PERFMON, or a VM with no PMU all yield `<not counted>`/`<not supported>` while `perf`
    # itself exits 0 — the exact shape of a gate that measures nothing and says it is fine.
    local probe
    probe="$(LC_ALL=C perf stat -x, -e instructions:u true 2>&1 >/dev/null \
        | awk -F, '$3 ~ /^instructions/ {print $1; exit}')"
    if [[ ! "$probe" =~ ^[0-9]+$ ]]; then
        local paranoid fix
        paranoid="$(cat /proc/sys/kernel/perf_event_paranoid 2> /dev/null || echo '?')"
        # Name the fix that matches the cause. Telling someone to lower `paranoid` when it is
        # already low sends them to change a setting that was never the problem — the remaining
        # explanations are a VM or container with no PMU exposed, or a missing CAP_PERFMON.
        if [[ "$paranoid" == "?" || "$paranoid" -gt 2 ]]; then
            fix="sudo sysctl kernel.perf_event_paranoid=2   # 2 is enough: this only counts :u (userspace)"
        else
            fix="perf_event_paranoid is already $paranoid, so this is not a permissions setting: the PMU is likely not exposed (a VM or container without one, or no CAP_PERFMON). There is no fallback — this ratchet gates on hardware counters or it does not gate."
        fi
        cannot "perf cannot count user instructions here (got '${probe:-<no counter line>}'; /proc/sys/kernel/perf_event_paranoid = $paranoid)" "$fix"
    fi

    # A baseline from another box is not something to compare against, in either direction — so it
    # belongs here, with the other "this machine cannot answer the question" conditions, rather
    # than in the failure path where it would read as a regression. Not in `--record`: re-recording
    # is the documented way OUT of a foreign baseline, so refusing to record because the baseline
    # is foreign would be a trap with no exit.
    local bfp
    [[ "$MODE" == record ]] && return 0
    if bfp="$(baseline_fingerprint)" && [[ -n "$bfp" && "$bfp" != "$(fingerprint)" ]]; then
        echo "perf-ratchet: baseline machine: $bfp" >&2
        echo "perf-ratchet: this machine:     $(fingerprint)" >&2
        cannot "the baseline was recorded on a different machine — instruction counts are not portable across microarchitectures or libc versions, so comparing them would be meaningless in both directions" \
            "scripts/perf-ratchet.sh --record   # if THIS machine is the one that should hold the baseline"
    fi
}

# Is the TREE in a state where a measurement is possible? Unlike the machine checks, none of these
# is a legitimate reason to skip: a missing binary is a build that did not happen, and a missing
# baseline or fixture is a gate that has quietly stopped gating. All hard failures.
tree_preflight() {
    [[ -x "$BIN" ]] \
        || cannot "no binary to measure at \`$BIN\`" \
            "cargo build --release -p noeta-cli   # or set NOETA_PERF_BIN=/path/to/noeta"

    # A debug binary is a different program: the baseline is recorded from a release build and
    # comparing across profiles is off by an order of magnitude, not by a tolerance.
    case "$BIN" in
        */release/* | */wasm-release/*) ;;
        *)
            if [[ -z "${NOETA_PERF_ALLOW_ANY_PROFILE:-}" ]]; then
                cannot "\`$BIN\` does not look like a release build (the baseline is recorded from one; a debug binary differs by an order of magnitude, not by a tolerance)" \
                    "cargo build --release -p noeta-cli   # or set NOETA_PERF_ALLOW_ANY_PROFILE=1 if you are sure"
            fi
            ;;
    esac

    local r id fx
    for r in "${ROWS[@]}"; do
        id="$(row_field "$r" 1)"
        [[ "$(row_field "$r" 5)" == *@* ]] || continue
        fx="$FIXTURES/$id/$id.noe"
        [[ -f "$fx" ]] \
            || cannot "row '$id' has no fixture at $fx" \
                "restore it, or drop the row from ROWS in $(basename "${BASH_SOURCE[0]}")"
    done

    if [[ "$MODE" != record ]]; then
        [[ -f "$BASELINE" ]] \
            || cannot "no baseline at $BASELINE" "scripts/perf-ratchet.sh --record"
    fi
}

machine_preflight

if [[ "$MODE" == preflight ]]; then
    echo "perf-ratchet: preflight OK — perf counts :u on $(fingerprint)"
    exit 0
fi

tree_preflight

# ----------------------------------------------------------------------------------- measurement

PIN=()
if command -v taskset > /dev/null 2>&1; then
    PIN=(taskset -c "$CPU")
else
    echo "perf-ratchet: NOTE — taskset not found; runs are unpinned, which widens loop_jit's spread" >&2
fi

# The private, CONSTANT-LENGTH staging root. See trap 3 in the header: the absolute path of the
# fixture is part of its instruction count, so the measurement must not inherit the length of the
# checkout it was run from. `/tmp` explicitly rather than $TMPDIR for the same reason — the point
# is that `/tmp/tmp.XXXXXXXXXX/<row>/<row>.noe` is the SAME LENGTH for every caller, which a
# caller-set TMPDIR would undo. (Per-row lengths still differ from each other; that is fine, each
# row is only ever compared against its own baseline.)
if [[ -d /tmp && -w /tmp ]]; then
    STAGE="$(TMPDIR=/tmp mktemp -d)" || cannot "mktemp -d under /tmp failed"
else
    STAGE="$(mktemp -d)" || cannot "mktemp -d failed"
    echo "perf-ratchet: NOTE — /tmp is not writable; staging under \$TMPDIR, whose path length" >&2
    echo "perf-ratchet:        is part of the measurement. Re-record if numbers shift ~0.1%." >&2
fi
trap 'rm -rf "$STAGE"' EXIT

stage_fixture() { # <row-id> -> prints the staged path
    local id="$1"
    mkdir -p "$STAGE/$id"
    cp "$FIXTURES/$id/$id.noe" "$STAGE/$id/$id.noe"
    printf '%s/%s/%s.noe' "$STAGE" "$id" "$id"
}

# argv for a row, with @ expanded to the staged fixture.
row_argv() { # <row-string> -> sets ARGV
    local r="$1" id fx a
    id="$(row_field "$r" 1)"
    ARGV=()
    for a in $(row_field "$r" 5); do
        if [[ "$a" == "@" ]]; then
            fx="$(stage_fixture "$id")"
            ARGV+=("$fx")
        else
            ARGV+=("$a")
        fi
    done
}

# min-of-REPS instructions retired. Returns empty when any repetition failed to produce a count —
# the caller treats that as "could not measure", not as a zero.
icount() {
    local best="" v
    for _ in $(seq "$REPS"); do
        v=$(LC_ALL=C perf stat -x, -e instructions:u "${PIN[@]}" "$@" 2>&1 > /dev/null \
            | awk -F, '$3 ~ /^instructions/ {print $1; exit}')
        [[ "$v" =~ ^[0-9]+$ ]] || continue
        if [[ -z "$best" || "$v" -lt "$best" ]]; then best="$v"; fi
    done
    printf '%s' "$best"
}

# What tier 1 did with this row: `native`, `declined`, or `none` (nothing hot enough to consider),
# together with the ops that blocked compilation — the finding a benchmark table cannot show you.
#
# It prints `state:ops` rather than setting globals: the caller reads this through `$( … )`, which
# is a SUBSHELL, so an assignment in here would never reach the caller. It did not, and the effect
# was silent — the state came back correctly and the blocked-by ops were simply always empty, i.e.
# the one piece of diagnosis this row exists to surface quietly went missing while the gate kept
# reporting PASS. Returning both through stdout is what makes that unrepresentable.
tier1_state() { # <bin> <verb> <rest...> -> prints "<native|declined|none>:<op op op>"
    local out compiled ops bin="$1" verb="$2"
    shift 2
    # `--jit-stats` goes immediately after the verb, not at the end: everything after the FILE
    # positional is the program's own argv, so appending it would pass the flag to the Noeta
    # program instead of to the CLI and this check would report `none` for every row.
    out="$("${PIN[@]}" "$bin" "$verb" --jit-stats "$@" 2>&1 > /dev/null)"
    # `  <file>:<line>  <Op>  <disassembly>` under the declined header; the `main — blocked by:`
    # line names the prototype, not an op.
    ops="$(printf '%s' "$out" | sed -n '/loops declined tier 1/,$p' | tail -n +2 \
        | awk 'NF && !/blocked by:/ {print $2}' | sort -u | tr '\n' ' ')"
    if printf '%s' "$out" | grep -q 'loops declined tier 1'; then
        printf 'declined:%s' "$ops"
        return
    fi
    compiled="$(printf '%s' "$out" | sed -n 's/^tier 1: \([0-9]*\) of .*/\1/p' | head -1)"
    if [[ "${compiled:-0}" -gt 0 ]]; then printf 'native:'; else printf 'none:'; fi
}

# ------------------------------------------------------------------------------------- the report

echo "perf-ratchet: binary   $BIN"
echo "perf-ratchet: machine  $(fingerprint)"
echo "perf-ratchet: method   instructions retired, min of $REPS, ${PIN[*]:-unpinned}, fixtures staged alone under $STAGE"
echo

# The baseline's numbers. Its `machine` line was already checked in machine_preflight, which is
# where a foreign baseline belongs: it stops the run rather than colouring one.
declare -A BASE_VAL=()
if [[ -f "$BASELINE" ]]; then
    while read -r kind id val _rest; do
        [[ "$kind" == row ]] && BASE_VAL["$id"]="$val"
    done < "$BASELINE"
fi

FAIL=0
UNMEASURED=0
declare -A GOT=() STATE=()
printf '%-10s %16s %16s %9s %8s  %s\n' row instructions baseline delta tol tier-1
printf -- '------------------------------------------------------------------------------------\n'

for r in "${ROWS[@]}"; do
    id="$(row_field "$r" 1)"
    tol="$(row_field "$r" 2)"
    want_tier="$(row_field "$r" 3)"
    want_out="$(row_field "$r" 4)"
    row_argv "$r"

    # Correctness first: a faster wrong answer is not a result. One un-instrumented run, compared
    # against the expected first line of stdout.
    if [[ -n "$want_out" ]]; then
        got_out="$("${PIN[@]}" "$BIN" "${ARGV[@]}" 2> /dev/null | head -1)"
        if [[ "$got_out" != "$want_out" ]]; then
            printf '%-10s  \033[31mWRONG OUTPUT\033[0m got=%s want=%s\n' "$id" "${got_out:-<none>}" "$want_out"
            FAIL=$((FAIL + 1))
            continue
        fi
    fi

    n="$(icount "$BIN" "${ARGV[@]}")"
    if [[ -z "$n" ]]; then
        printf '%-10s  \033[31mNO COUNTER\033[0m — every repetition failed to produce an instruction count\n' "$id"
        UNMEASURED=$((UNMEASURED + 1))
        continue
    fi
    GOT["$id"]="$n"

    tier="n/a"
    DECLINED_OPS=""
    if [[ "$want_tier" != "n/a" ]]; then
        t1="$(tier1_state "$BIN" "${ARGV[@]}")"
        tier="${t1%%:*}"
        DECLINED_OPS="${t1#*:}"
        STATE["$id"]="$tier"
    fi

    base="${BASE_VAL[$id]:-}"
    delta="-"
    verdict=""
    if [[ "$MODE" == compare ]]; then
        if [[ -z "$base" ]]; then
            printf '%-10s %16s %16s %9s %8s  %s\n' "$id" "$n" "(none)" "-" "$tol%" "$tier"
            echo "           ^ the baseline has no entry for this row, so nothing was compared." >&2
            UNMEASURED=$((UNMEASURED + 1))
            continue
        fi
        pct="$(awk -v a="$n" -v b="$base" 'BEGIN {printf "%.2f", (a-b)/b*100}')"
        lim="$(awk -v t="$tol" -v s="$TOL_SCALE" 'BEGIN {printf "%.4f", t*s}')"
        delta="$(printf '%+.2f%%' "$pct")"
        over="$(awk -v p="$pct" -v l="$lim" 'BEGIN {print (p > l) ? "slower" : ((p < -l) ? "faster" : "ok")}')"
        case "$over" in
            slower)
                verdict="REGRESSION"
                FAIL=$((FAIL + 1))
                ;;
            faster)
                if ((ALLOW_IMPROVEMENT)); then
                    verdict="improved"
                else
                    verdict="IMPROVED — re-record"
                    FAIL=$((FAIL + 1))
                fi
                ;;
        esac
    fi

    color="" reset=""
    [[ "$verdict" == REGRESSION* ]] && color='\033[31m' reset='\033[0m'
    [[ "$verdict" == IMPROVED* ]] && color='\033[33m' reset='\033[0m'
    printf "%-10s %16s %16s ${color}%9s${reset} %8s  %s%s\n" \
        "$id" "$n" "${base:-(none)}" "$delta" "$tol%" "$tier" \
        "$([[ -n "$verdict" ]] && printf '  <- %s' "$verdict")"

    # The tier-1 expectation. This is the check that three benchmark reports missed: when a
    # benchmark's JIT and interpreter numbers are equal, the JIT is not running on it, and that is
    # the finding. It fails in BOTH directions on purpose — a row that starts compiling natively
    # is good news whose pinned number now measures a different program.
    if [[ "$want_tier" != "n/a" && "$tier" != "$want_tier" ]]; then
        FAIL=$((FAIL + 1))
        printf '           \033[31mTIER-1 CHANGED\033[0m  expected %s, got %s\n' "$want_tier" "$tier"
        case "$tier" in
            declined)
                printf '           this row now runs INTERPRETED. Ops that blocked tier 1: %s\n' \
                    "${DECLINED_OPS:-<none reported>}"
                printf '           (reproduce: %s run --jit-stats %s)\n' "$BIN" "${ARGV[*]: -1}"
                ;;
            native)
                printf '           this row now COMPILES natively — an improvement, and its pinned\n'
                printf '           number is no longer measuring the interpreter. Re-record.\n'
                ;;
            none)
                printf '           nothing was compiled AND nothing declined: either the binary has\n'
                printf '           no JIT (built --no-default-features?) or the loop went cold.\n'
                ;;
        esac
    elif [[ "$tier" == declined && -n "$DECLINED_OPS" ]]; then
        printf '           interpreted by design; tier 1 blocked by: %s\n' "$DECLINED_OPS"
    fi
done

echo

# ----------------------------------------------------------------------------------------- record

if [[ "$MODE" == record ]]; then
    if ((FAIL || UNMEASURED)); then
        echo "perf-ratchet: refusing to record — $FAIL row(s) failed and $UNMEASURED could not be measured." >&2
        echo "perf-ratchet: a baseline recorded from a broken run pins the breakage." >&2
        exit 2
    fi
    {
        echo "# Baseline for scripts/perf-ratchet.sh — INSTRUCTIONS RETIRED, min of $REPS."
        echo "#"
        echo "# Recorded $(date -u +%Y-%m-%dT%H:%M:%SZ) from $BIN"
        echo "# Re-record with:  scripts/perf-ratchet.sh --record"
        echo "#"
        echo "# Read the diff, do not just accept it: numbers DOWN is an improvement worth keeping"
        echo "# (the ratchet then holds the new floor); numbers UP needs a sentence in the commit"
        echo "# message saying which change bought it and what for."
        echo "#"
        echo "# \`machine\` is not decoration: instruction counts are not portable across"
        echo "# microarchitectures or libc versions, so a mismatch makes the gate report CANNOT"
        echo "# MEASURE (exit 2) rather than inventing a regression."
        echo
        echo "machine $(fingerprint)"
        echo
        for r in "${ROWS[@]}"; do
            id="$(row_field "$r" 1)"
            printf 'row %-10s %16s\n' "$id" "${GOT[$id]}"
        done
    } > "$BASELINE"
    echo "perf-ratchet: recorded $BASELINE"
    exit 0
fi

# ---------------------------------------------------------------------------------------- verdict

if ((UNMEASURED)); then
    echo "perf-ratchet: $UNMEASURED row(s) produced NO comparison. That is not a pass." >&2
    echo "perf-ratchet: a row with no baseline entry needs:  scripts/perf-ratchet.sh --record" >&2
    exit 2
fi

if ((FAIL)); then
    cat >&2 <<EOF
perf-ratchet: FAILED — $FAIL row(s) outside their band or with a changed tier-1 state.

  Is it a regression or an intended improvement? Read the delta's SIGN in the table above.
    +N%  slower than the baseline. Unless you know which change bought it and what for, this is
         the regression this gate exists to catch. Bisect with:
             scripts/perf-ratchet.sh --bin <binary built from the suspect commit>
         (any binary works — the gate measures what you point it at.)
    -N%  faster. Good. Re-record so the ratchet holds the new floor:
             scripts/perf-ratchet.sh --record
         Then commit tests/perf/baseline.txt with the rest of the change.

  A changed TIER-1 state is never "just noise": it means the row is running on a different engine
  than the day its number was recorded, so the number and the new engine do not belong together.

  Both bands and both directions are documented at the top of scripts/perf-ratchet.sh, including
  the measured variance each tolerance came from. Widen a band with NOETA_PERF_TOL_SCALE if this
  machine is genuinely noisier than the one the baseline came from — but widening is a decision,
  and it belongs in a commit message.
EOF
    exit 1
fi

echo "perf-ratchet: PASSED — every row inside its band, every tier-1 expectation held."
exit 0
