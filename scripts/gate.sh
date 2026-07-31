#!/usr/bin/env bash
# The pre-merge gate — run what `.github/workflows/ci.yml` runs, locally, before merging to `main`.
#
# Why this exists: `main` here is developed in parallel worktrees and is never pushed, so CI never
# actually executes. The gates in ci.yml are the right gates and nothing runs them, which is how
# `main` twice sat red under `clippy -D warnings` — each time found by accident, by an agent doing
# unrelated work. This script is the thing you run instead of the CI that will not run for you.
#
# It is built to be hard to get a false pass from. A postmortem on one of those red commits found
# the local check had *reported a pass* because clippy was piped through `tail` and the exit code
# read was the pipe's. So: no step's output is ever piped, every step's status is read directly and
# recorded, a step that cannot run is reported SKIP (never PASS), and the summary is a per-step
# table with the overall verdict at the bottom. `set -e` is deliberately OFF — one red step must not
# hide the state of the rest; everything runs, then the script exits non-zero.
#
#   scripts/gate.sh --quick     the inner loop  — fmt + both clippy splits          (~1 min warm)
#   scripts/gate.sh             the merge gate  — + tests, doc samples, JIT oracles (~14 min warm)
#   scripts/gate.sh --full      full CI parity  — + wasm, miri, editors, e2e        (~1 h+, cold)
#
# Those timings are MEASURED on a 20-core box with a warm CARGO_TARGET_DIR, except `--full`, whose
# wasm/miri legs build against targets and a toolchain a normal dev loop never warms — budget an
# hour and a lot of disk the first time. A cold target dir adds ~10-20 min to any tier. If the cost
# surprises you, you will skip the gate, and a skipped gate is exactly what we already have.
#
# Options:
#   --quick / --full        pick a tier (default: the merge gate, in between)
#   --only <substring>      run only steps whose group or name matches (e.g. --only clippy)
#   --list                  print the plan for the chosen tier and exit
#   --install-hook [--force]  install an OPT-IN .git/hooks/pre-push running --quick (see below)
#   -h | --help             this header
#
# Environment:
#   NOETA_GATE_TOOLCHAIN    rustup toolchain to gate with. Defaults to the CI pin `1.97.0` when it
#                           is installed, else the default toolchain. Clippy's lint set is
#                           version-sensitive: gating on a floating stable will disagree with CI.
#                           Set to `default` to use whatever `cargo` resolves to. NOTE that a
#                           second toolchain means a second set of build artifacts in your target
#                           dir — the first run after switching rebuilds the world.
#   CARGO_TARGET_DIR        respected, not set. Use a per-agent one (see AGENTS.md); a shared
#                           target dir produces phantom failures this script would faithfully
#                           report as real.
#   CARGO_BUILD_JOBS        respected, not set. With several agents on one box, divide the cores.
#
# On the hook: `--install-hook` exists, is opt-in, never runs on its own, and will not clobber an
# existing hook without --force. It is NOT recommended as this repo's primary mechanism, for three
# reasons. (1) The moment that matters here is the merge into `main`, and `pre-merge-commit` does
# not fire on a fast-forward merge — which is how most work lands — so the hook would silently not
# run exactly when you needed it, while reading as coverage. (2) Hooks live in the *common* git
# dir, shared by every worktree of this repo, so one agent installing one changes every other
# agent's `git push`. (3) `--no-verify` makes any hook advisory. The gate's real enforcement is
# social and documented (AGENTS.md, CONTRIBUTING.md): run it, green, before you merge.
#
# Not run by any tier, because it builds a second full toolchain (tens of GB, tens of minutes) and
# CI reclaims 25 GB of runner disk to do it: the composed-toolchain e2e
# (`cargo test -p noeta-cli --no-default-features -- --ignored composed_toolchain`). Run it by hand
# when you touch `noeta-pm`'s native-package composition. It is the one CI step this script omits.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

TIER=2 # 1 = --quick, 2 = merge gate (default), 3 = --full
ONLY=""
LIST_ONLY=0
INSTALL_HOOK=0
FORCE=0

usage() {
    sed -n '2,/^set -uo/p' "${BASH_SOURCE[0]}" | sed -e 's/^# \{0,1\}//' -e '$d'
}

while (($#)); do
    case "$1" in
        --quick) TIER=1 ;;
        --full) TIER=3 ;;
        --only)
            ONLY="${2:?--only needs a substring}"
            shift
            ;;
        --list) LIST_ONLY=1 ;;
        --install-hook) INSTALL_HOOK=1 ;;
        --force) FORCE=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "gate: unknown option '$1' (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

# ---------------------------------------------------------------------------- toolchain selection

TC="${NOETA_GATE_TOOLCHAIN:-}"
if [[ -z "$TC" ]]; then
    if rustup toolchain list 2>/dev/null | grep -q '^1\.97\.0-'; then
        TC="1.97.0"
    else
        TC="default"
        echo "gate: WARNING — the CI pin 1.97.0 is not installed; gating on the default toolchain." >&2
        echo "gate:           clippy's lint set is version-sensitive, so this can disagree with CI." >&2
        echo "gate:           install it with: rustup toolchain install 1.97.0 --component clippy,rustfmt" >&2
    fi
fi
if [[ "$TC" == "default" ]]; then
    CARGO=(cargo)
else
    CARGO=(cargo "+$TC")
fi

# ------------------------------------------------------------------------------ the opt-in hook

install_hook() {
    local hooks
    hooks="$(git rev-parse --git-path hooks)" || exit 2
    mkdir -p "$hooks"
    local target="$hooks/pre-push"
    if [[ -e "$target" && $FORCE -eq 0 ]]; then
        echo "gate: $target already exists — inspect it, then re-run with --force to replace it." >&2
        exit 2
    fi
    cat > "$target" <<'HOOK'
#!/usr/bin/env bash
# Installed by scripts/gate.sh --install-hook. Runs the QUICK tier (fmt + clippy) before a push.
# Git does not share hooks between clones, so this is per-checkout and opt-in by design.
# Bypass with `git push --no-verify` when you know what you are doing.
exec "$(git rev-parse --show-toplevel)/scripts/gate.sh" --quick
HOOK
    chmod +x "$target"
    echo "gate: installed $target (runs --quick; bypass with git push --no-verify)"
    echo "gate: NOTE — hooks live in the common git dir, so this now applies to EVERY worktree of"
    echo "gate:        this repo, including other agents'. It also does not fire on a merge into"
    echo "gate:        \`main\`, which is the moment that actually matters here. Remove with:"
    echo "gate:        rm $target"
}

if ((INSTALL_HOOK)); then
    install_hook
    exit 0
fi

# ------------------------------------------------------------------------------------ step runner

# Per-run log directory. Not a fixed path: two gate runs sharing one directory would overwrite each
# other's logs and you would read someone else's failure under your own step's name — the same trap
# AGENTS.md documents for shared scratch dirs. `latest` points at the most recent run.
LOGBASE="${CARGO_TARGET_DIR:-$ROOT/target}/gate-logs"
LOGDIR="$LOGBASE/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$LOGDIR" || exit 2
ln -sfn "$LOGDIR" "$LOGBASE/latest"

declare -a S_GROUP=() S_NAME=() S_STATUS=() S_SECS=() S_LOG=() S_CMD=() S_NOTE=()
STEP_N=0
FAILED=0

# selected <tier> <group> <name> -> 0 when this step is in scope
selected() {
    local tier="$1" group="$2" name="$3"
    ((tier > TIER)) && return 1
    if [[ -n "$ONLY" && "$group" != *"$ONLY"* && "$name" != *"$ONLY"* ]]; then return 1; fi
    return 0
}

record() { # <group> <name> <status> <secs> <log> <cmd> <note>
    S_GROUP+=("$1")
    S_NAME+=("$2")
    S_STATUS+=("$3")
    S_SECS+=("$4")
    S_LOG+=("$5")
    S_CMD+=("$6")
    S_NOTE+=("$7")
}

# step <tier> <group> <name> -- <command...>
step() {
    local tier="$1" group="$2" name="$3"
    shift 3
    [[ "${1:-}" == "--" ]] && shift
    selected "$tier" "$group" "$name" || return 0

    STEP_N=$((STEP_N + 1))
    local slug log pretty
    slug="$(printf '%02d-%s' "$STEP_N" "$name" | tr -c 'A-Za-z0-9._-' '-')"
    log="$LOGDIR/$slug.log"
    pretty="$(shellish "$@")"

    if ((LIST_ONLY)); then
        printf '  [%-7s] %-52s %s\n' "$group" "$name" "$pretty"
        return 0
    fi

    printf '\n\033[1m== [%s] %s\033[0m\n' "$group" "$name"
    printf '   $ %s\n' "$pretty"
    local t0=$SECONDS
    # No pipe: the command's own exit status is what we read. Output goes to a file and is
    # replayed from there on failure. This is the whole point of the script.
    "$@" > "$log" 2>&1
    local rc=$?
    local secs=$((SECONDS - t0))

    if ((rc == 0)); then
        printf '   \033[32mPASS\033[0m  %s  (log: %s)\n' "$(fmt_dur "$secs")" "$log"
        record "$group" "$name" PASS "$secs" "$log" "$pretty" ""
    else
        FAILED=$((FAILED + 1))
        printf '   \033[31mFAIL\033[0m  %s  exit %d\n' "$(fmt_dur "$secs")" "$rc"
        echo   "   ---- last 40 lines of $log ----"
        tail -n 40 "$log" | sed 's/^/   | /'
        echo   "   ---- end ----"
        record "$group" "$name" FAIL "$secs" "$log" "$pretty" "exit $rc"
    fi
}

# skip <tier> <group> <name> <reason>
skip() {
    local tier="$1" group="$2" name="$3" reason="$4"
    selected "$tier" "$group" "$name" || return 0
    STEP_N=$((STEP_N + 1))
    if ((LIST_ONLY)); then
        printf '  [%-7s] %-52s (SKIP: %s)\n' "$group" "$name" "$reason"
        return 0
    fi
    printf '\n\033[1m== [%s] %s\033[0m\n   \033[33mSKIP\033[0m  %s\n' "$group" "$name" "$reason"
    record "$group" "$name" SKIP 0 "" "" "$reason"
}

# Render a command array as a line you can actually paste back into a shell. `$*` would drop the
# quoting on `--features "noeta-vm/jit noeta-conformance/jit"` and the pasted command would then
# mean something different from the one that failed.
shellish() {
    local out="" a
    for a in "$@"; do
        if [[ "$a" =~ [[:space:]\'\"\$\&\|\<\>\(\)] ]]; then
            out+="'${a//\'/\'\\\'\'}' "
        else
            out+="$a "
        fi
    done
    printf '%s' "${out% }"
}

fmt_dur() {
    local s="$1"
    if ((s < 60)); then printf '%2ds' "$s"; else printf '%dm%02ds' $((s / 60)) $((s % 60)); fi
}

have() { command -v "$1" > /dev/null 2>&1; }

# Is a cross-compilation target installed for the toolchain we are gating with? (A target installed
# for `stable` does nothing for a `+1.97.0` build, so ask about the right toolchain.)
has_target() {
    if [[ "$TC" == "default" ]]; then
        rustup target list --installed 2>/dev/null | grep -qx "$1"
    else
        rustup target list --installed --toolchain "$TC" 2>/dev/null | grep -qx "$1"
    fi
}

# ------------------------------------------------------------------------------------- the plan
#
# The split mirrors ci.yml job-for-job, and the tiers are cut along "what has actually gone red".
#
#   fmt + clippy  (tier 1, --quick)  — the two lints that have twice landed a red `main`, and the
#                                     cheapest things that can. Fast enough for the inner loop.
#   test/docs/jit (tier 2, default)  — the oracles: conformance, eval↔VM differential, leak, the
#                                     doc samples, and the JIT's own differential. This is the set
#                                     a merge to `main` must clear.
#   wasm/miri/editors (tier 3)       — portability, `unsafe` soundness, and the editor grammars.
#                                     Real gates, but they need wasmtime/nightly+miri/npm and they
#                                     dominate the runtime, so they are opt-in per merge, not per
#                                     commit. `--full` is what you run before a release tag.

if ((LIST_ONLY)); then
    echo "gate: plan for tier $TIER (toolchain: $TC)"
fi

# --- fmt --------------------------------------------------------------------------------------
step 1 fmt "cargo fmt --all --check" -- "${CARGO[@]}" fmt --all --check

# --- clippy (deny warnings), split exactly as ci.yml splits it ---------------------------------
# Default build excludes noeta-jit and noeta-cli (both pull Cranelift — noeta-cli via its default
# `jit` feature); the second invocation lints those under the feature.
step 1 clippy "clippy (default features)" -- \
    "${CARGO[@]}" clippy --workspace --exclude noeta-jit --exclude noeta-cli --all-targets --locked -- -D warnings
step 1 clippy "clippy (jit feature)" -- \
    "${CARGO[@]}" clippy -p noeta-vm -p noeta-jit -p noeta-conformance -p noeta-cli \
    --features "noeta-vm/jit noeta-conformance/jit" --all-targets --locked -- -D warnings

# --- test: the workspace suite + the oracles ---------------------------------------------------
step 2 test "cargo test --workspace (Cranelift-free)" -- \
    "${CARGO[@]}" test --workspace --exclude noeta-jit --exclude noeta-cli --locked
step 2 test "lean CLI build (--no-default-features)" -- \
    "${CARGO[@]}" test -p noeta-cli --no-default-features --locked
# The lean feature SHAPES the AOT/native-size/p2p stories depend on. ci.yml runs these as one
# step; split here so a failure names the shape instead of a line number in a shell block.
step 2 test "shape: noeta-vm aot" -- \
    "${CARGO[@]}" check -p noeta-vm --no-default-features --features aot --locked
step 2 test "shape: noeta-vm jit,aot" -- \
    "${CARGO[@]}" check -p noeta-vm --no-default-features --features jit,aot --locked
step 2 test "shape: noeta-host-real (no default)" -- \
    "${CARGO[@]}" check -p noeta-host-real --no-default-features --locked
step 2 test "shape: noeta-stdlib (no default)" -- \
    "${CARGO[@]}" check -p noeta-stdlib --no-default-features --locked

# --- docs: every ```noeta block in docs/ runs through the real binary --------------------------
step 2 docs "doc samples (docs/*.md)" -- \
    "${CARGO[@]}" test -p noeta-cli --test doc_samples --no-default-features --locked

# --- jit: the tier-0/tier-1 differential + leak-under-JIT --------------------------------------
step 2 jit "JIT unit tests (noeta-vm)" -- \
    "${CARGO[@]}" test -p noeta-vm --features jit --locked
step 2 jit "JIT differential + leak corpus gate" -- \
    "${CARGO[@]}" test -p noeta-conformance --features jit --locked
step 2 jit "JIT differential (coverage summary)" -- \
    "${CARGO[@]}" run -p noeta-conformance --features jit --locked -- --jit-differential
step 2 jit "JIT differential (cancel-poll arm)" -- \
    "${CARGO[@]}" run -p noeta-conformance --features jit --locked -- --jit-differential --cancel-poll
step 2 jit "JIT-enabled CLI (integration + doc samples)" -- \
    "${CARGO[@]}" test -p noeta-cli --locked

# --- wasm: the portability invariant + the wasm/browser/serve oracles --------------------------
if [[ "$TC" == "default" ]]; then
    TARGET_HINT="target not installed — rustup target add"
else
    TARGET_HINT="target not installed for toolchain $TC — rustup target add --toolchain $TC"
fi
WASMTIME="${NOETA_WASMTIME:-}"
if [[ -z "$WASMTIME" ]] && have wasmtime; then WASMTIME="$(command -v wasmtime)"; fi

if has_target wasm32-wasip1; then
    step 3 wasm "check pipeline crates (wasm32-wasip1)" -- \
        "${CARGO[@]}" check -p noeta-vm -p noeta-stdlib -p noeta-compiler -p noeta-lexer \
        -p noeta-parser -p noeta-loader -p noeta-db -p noeta-bundle -p noeta-eval \
        --target wasm32-wasip1 --locked
    step 3 wasm "build the wasm runner (wasm-release)" -- \
        "${CARGO[@]}" build -p noeta-wasm-runner --target wasm32-wasip1 --profile wasm-release --locked
else
    # Note the toolchain: the wasm targets are commonly installed for `stable` only, and this gate
    # defaults to the `+1.97.0` CI pin — a target added to the wrong toolchain looks like it is
    # there and is not.
    skip 3 wasm "check pipeline crates (wasm32-wasip1)" "$TARGET_HINT wasm32-wasip1"
    skip 3 wasm "build the wasm runner (wasm-release)" "$TARGET_HINT wasm32-wasip1"
fi

if [[ -n "$WASMTIME" ]] && has_target wasm32-wasip1; then
    step 3 wasm "wasm differential oracle (runner vs native VM)" -- \
        env "NOETA_WASMTIME=$WASMTIME" "${CARGO[@]}" run -p noeta-conformance --locked -- --wasm-differential
else
    skip 3 wasm "wasm differential oracle (runner vs native VM)" "wasmtime not on PATH (set NOETA_WASMTIME)"
fi

if has_target wasm32-unknown-unknown; then
    step 3 wasm "build the playground engine (wasm32-unknown-unknown)" -- \
        "${CARGO[@]}" build -p noeta-playground --target wasm32-unknown-unknown --profile wasm-release --locked
    if have node; then
        step 3 wasm "browser-engine smoke (node over the raw ABI)" -- \
            node crates/noeta-playground/tests/browser_smoke.mjs \
            "${CARGO_TARGET_DIR:-$ROOT/target}/wasm32-unknown-unknown/wasm-release/noeta_playground.wasm"
    else
        skip 3 wasm "browser-engine smoke (node over the raw ABI)" "node not on PATH"
    fi
else
    skip 3 wasm "build the playground engine (wasm32-unknown-unknown)" "$TARGET_HINT wasm32-unknown-unknown"
    skip 3 wasm "browser-engine smoke (node over the raw ABI)" "$TARGET_HINT wasm32-unknown-unknown"
fi

if [[ -n "$WASMTIME" ]] && has_target wasm32-wasip2; then
    step 3 wasm "wasi:http serve e2e (component under wasmtime serve)" -- \
        env "NOETA_WASMTIME=$WASMTIME" "PATH=$(dirname "$WASMTIME"):$PATH" \
        bash crates/noeta-wasm-serve/tests/e2e.sh
else
    skip 3 wasm "wasi:http serve e2e (component under wasmtime serve)" "needs wasmtime + target wasm32-wasip2"
fi

# --- miri: the one place `unsafe` lives -------------------------------------------------------
if rustup component list --toolchain nightly --installed 2>/dev/null | grep -q '^miri'; then
    step 3 miri "cargo miri test (noeta-value, noeta-gc)" -- \
        env PROPTEST_CASES=16 MIRIFLAGS=-Zmiri-disable-isolation \
        cargo +nightly miri test -p noeta-value -p noeta-gc --locked
else
    skip 3 miri "cargo miri test (noeta-value, noeta-gc)" "nightly miri not installed (rustup +nightly component add miri)"
fi

# --- editors: tree-sitter grammar + the VS Code extension's TextMate tests ---------------------
if have npm; then
    step 3 editors "VS Code extension: npm ci" -- env -C "$ROOT/editors/vscode-noeta" npm ci
    step 3 editors "VS Code extension: npm test" -- env -C "$ROOT/editors/vscode-noeta" npm test
    step 3 editors "tree-sitter: npm ci" -- env -C "$ROOT/editors/tree-sitter-noeta" npm ci
    # src/parser.c is generated and gitignored — generate before testing, like CI does.
    step 3 editors "tree-sitter: npm run generate" -- env -C "$ROOT/editors/tree-sitter-noeta" npm run generate
    step 3 editors "tree-sitter: npm test" -- env -C "$ROOT/editors/tree-sitter-noeta" npm test
else
    skip 3 editors "editor tooling (tree-sitter + VS Code)" "npm not on PATH"
fi

# ---------------------------------------------------------------------------------- the summary

if ((LIST_ONLY)); then exit 0; fi

TOTAL=$SECONDS
echo
echo "================================================================================"
printf ' gate summary — tier %s, toolchain %s, %s\n' \
    "$([[ $TIER == 1 ]] && echo quick || { [[ $TIER == 3 ]] && echo full || echo merge; })" \
    "$TC" "$(fmt_dur "$TOTAL") total"
echo "================================================================================"

n_pass=0 n_fail=0 n_skip=0
for i in "${!S_NAME[@]}"; do
    case "${S_STATUS[$i]}" in
        PASS)
            n_pass=$((n_pass + 1))
            color='\033[32m'
            ;;
        FAIL)
            n_fail=$((n_fail + 1))
            color='\033[31m'
            ;;
        *)
            n_skip=$((n_skip + 1))
            color='\033[33m'
            ;;
    esac
    printf " ${color}%-4s\033[0m %7s  [%-7s] %s%s\n" \
        "${S_STATUS[$i]}" "$(fmt_dur "${S_SECS[$i]}")" "${S_GROUP[$i]}" "${S_NAME[$i]}" \
        "$([[ -n "${S_NOTE[$i]}" ]] && echo " — ${S_NOTE[$i]}")"
done

echo "--------------------------------------------------------------------------------"
printf ' %d passed, %d failed, %d skipped\n' "$n_pass" "$n_fail" "$n_skip"

if ((n_fail)); then
    echo
    echo ' Reproduce the failures:'
    for i in "${!S_NAME[@]}"; do
        [[ "${S_STATUS[$i]}" == FAIL ]] || continue
        printf '   %s\n     log: %s\n' "${S_CMD[$i]}" "${S_LOG[$i]}"
    done
    echo
    echo ' GATE FAILED — do not merge.'
    exit 1
fi

if ((n_skip)); then
    echo
    echo ' NOTE: a SKIP is not a PASS. The steps above did not run; install their prerequisites'
    echo '       (or accept that CI would be the first thing to execute them) before a release tag.'
fi

echo
echo ' GATE PASSED.'
exit 0
