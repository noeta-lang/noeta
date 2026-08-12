#!/usr/bin/env bash
# The pre-merge gate — run what `.github/workflows/ci.yml` runs, locally, before merging to `main`.
#
# Why this exists: `main` here is developed in parallel worktrees and pushed in batches, so
# `.github/workflows/ci.yml` runs — but only at push cadence, which can be many merges later. The
# gates in ci.yml are the right gates; what is missing is anything running them *between* merges.
# That gap is not theoretical: `main` twice sat red under `clippy -D warnings` in a single day, each
# time found by accident by an agent doing unrelated work, and a `noeta-mcp` test sat red for over
# two hours. CI would have caught all three — at the next push, after the bad merges had already
# been built on. This script moves that feedback to the merge itself.
#
# It is built to be hard to get a false pass from. A postmortem on one of those red commits found
# the local check had *reported a pass* because clippy was piped through `tail` and the exit code
# read was the pipe's. So: no step's output is ever piped, every step's status is read directly and
# recorded, a step that cannot run is reported SKIP (never PASS), and the summary is a per-step
# table with the overall verdict at the bottom. `set -e` is deliberately OFF — one red step must not
# hide the state of the rest; everything runs, then the script exits non-zero.
#
#   scripts/gate.sh --quick     the inner loop  — fmt + both clippy splits           1m20s / 2m10s
#   scripts/gate.sh             the merge gate  — + tests, doc samples, JIT oracles,
#                                                 the perf ratchet                     ~20m / 42m
#   scripts/gate.sh --full      full CI parity  — + wasm, AOT, miri, editor tooling    +2m and up
#
# The merge tier also runs the `#[ignore]`d real-socket suites (`scripts/hot-e2e.sh` — hot reload,
# LiveView, graceful drain) against the JIT-enabled CLI it has already built. Measured at 5s warm,
# 0 failures in 62 consecutive runs including 24 at eight-way concurrency on a saturated box, so it
# does not move the numbers below; it is listed here because a step nobody knows about is a step
# nobody maintains.
#
# Those are MEASURED wall times on a 20-core box, `warm target dir / cold target dir`, with
# CARGO_BUILD_JOBS=8. The two long poles in the merge gate are `cargo test --workspace` (4m50s warm,
# 9m53s cold) and the lean-CLI build (8m warm, 9m cold — it is a second, Cranelift-free link of the
# whole CLI); the JIT group adds 13m cold and a few minutes warm. The perf group adds a THIRD link
# of the CLI — a `--release` one, codegen-units=1 + thin LTO, ~6m cold on this box under load —
# and the ratchet it feeds then runs in seconds. `--full` adds miri (1m12s, 63
# tests) and the editor tooling (4s) — cheap. Its wasm legs were NOT measured here: they need
# wasmtime and the wasm targets installed *for the gating toolchain*, and the runner / playground /
# component builds behind them are the expensive part when they do run.
#
# Budget accordingly, and do not be surprised: a gate whose cost is a surprise gets skipped, and a
# skipped gate is exactly what we already have.
#
# On SKIP: a step whose prerequisite is missing prints the exact missing piece and the command that
# fixes it, and the summary lists every skipped step in full rather than as a one-line footnote.
# This is not cosmetic. The wasm differential oracle sat red while reporting SKIP, and the reason it
# printed ("wasmtime not on PATH") was wrong twice over: wasmtime WAS installed, in `~/.wasmtime/bin`
# where its own installer puts it and where a non-interactive shell never looks, and the real gap was
# `wasm32-wasip1` being installed for `stable` but not for the pinned `1.97.0`. A gate that cannot
# run has to be loud about precisely why, and in CI it should not be a SKIP at all — see
# NOETA_GATE_REQUIRE_TOOLS below.
#
# Options:
#   --quick / --full        pick a tier (default: the merge gate, in between)
#   --only <substring>      run only steps whose group or name matches (e.g. --only clippy).
#                           Overrides the tier: naming a step is the selection, so `--only wasm`
#                           runs the wasm steps without also needing --full.
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
#   NOETA_GATE_REQUIRE_TOOLS  1 = a missing prerequisite is a FAIL, not a SKIP. Defaults to 1 when
#                           `$CI` is set, else 0. The asymmetry is deliberate: on a dev box not
#                           everyone has wasmtime / nightly miri / npm, and hard-failing there would
#                           make `--full` unrunnable and push people back to `--quick` forever. In
#                           an environment that INSTALLS the tooling, "prerequisite missing" does
#                           not mean "unavailable" — it means the install or the detection broke,
#                           and that must never read as a pass.
#   NOETA_WASMTIME          path to the wasmtime binary. Not normally needed: the gate probes
#                           `$PATH` and then the usual install dirs (`~/.wasmtime/bin`, …).
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

# Whether a missing prerequisite is a failure rather than a SKIP. Defaults ON when `$CI` is set:
# an automated environment installs the tooling it means to exercise, so "wasmtime not found" there
# is a broken install step, not an absent tool — and a SKIP that reads as a pass is exactly how the
# wasm differential stayed red without anyone noticing. Force either way with 0/1.
REQUIRE_TOOLS="${NOETA_GATE_REQUIRE_TOOLS:-}"
if [[ -z "$REQUIRE_TOOLS" ]]; then
    if [[ -n "${CI:-}" ]]; then REQUIRE_TOOLS=1; else REQUIRE_TOOLS=0; fi
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
#
# `--only` overrides the tier, deliberately: naming a step IS the selection, and asking for one that
# lives above the current tier should run it rather than silently match nothing. `--only wasm`
# without `--full` used to report zero steps — honest since the vacuous-pass fix, but the reason it
# gave ("no step matched, in tier 2") named a tier the caller had not thought about, on a group whose
# whole history is of not running. If you asked for it by name, you get it.
selected() {
    local tier="$1" group="$2" name="$3"
    if [[ -n "$ONLY" ]]; then
        [[ "$group" == *"$ONLY"* || "$name" == *"$ONLY"* ]] && return 0
        return 1
    fi
    ((tier > TIER)) && return 1
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

# skip <tier> <group> <name> <reason> [fix-command]
#
# A step whose prerequisite is missing. On a dev box that is a SKIP: not everyone has wasmtime, a
# nightly miri, or npm, and hard-failing there would make `--full` unrunnable and push people back
# to `--quick` forever — the opposite of the point. Under REQUIRE_TOOLS it is a FAIL instead: in an
# environment that installs the tooling (CI), "prerequisite missing" does not mean "not available",
# it means the detection or the install step broke, and that must not read as a pass.
skip() {
    local tier="$1" group="$2" name="$3" reason="$4" fix="${5:-}"
    selected "$tier" "$group" "$name" || return 0
    STEP_N=$((STEP_N + 1))
    if ((LIST_ONLY)); then
        printf '  [%-7s] %-52s (SKIP: %s)\n' "$group" "$name" "$reason"
        return 0
    fi
    printf '\n\033[1m== [%s] %s\033[0m\n' "$group" "$name"
    if ((REQUIRE_TOOLS)); then
        FAILED=$((FAILED + 1))
        printf '   \033[31mMISSING\033[0m  %s\n' "$reason"
        printf '   (NOETA_GATE_REQUIRE_TOOLS is on: a missing prerequisite is a failure here.)\n'
        record "$group" "$name" FAIL 0 "" "$fix" "prerequisite missing: $reason"
        return 0
    fi
    printf '   \033[33mSKIP\033[0m  %s\n' "$reason"
    [[ -n "$fix" ]] && printf '   fix: %s\n' "$fix"
    record "$group" "$name" SKIP 0 "" "$fix" "$reason"
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

# Find wasmtime. `$PATH` alone is not enough: wasmtime's own installer (the documented way to get
# it, https://wasmtime.dev/install.sh) drops the binary in `~/.wasmtime/bin` and edits a shell rc
# file — so a non-interactive `bash scripts/gate.sh` never sees it, and the differential oracle
# reported SKIP on a machine where wasmtime was installed and working. Probe the install locations
# that actually exist before concluding it is missing.
find_wasmtime() {
    if [[ -n "${NOETA_WASMTIME:-}" ]]; then
        printf '%s' "$NOETA_WASMTIME"
        return 0
    fi
    if have wasmtime; then
        command -v wasmtime
        return 0
    fi
    local candidate
    for candidate in \
        "$HOME/.wasmtime/bin/wasmtime" \
        "$HOME/.cargo/bin/wasmtime" \
        "$HOME/.local/bin/wasmtime" \
        /usr/local/bin/wasmtime \
        /opt/homebrew/bin/wasmtime \
        /opt/wasmtime/bin/wasmtime; do
        if [[ -x "$candidate" ]]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

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
#                                     check-vs-run project differential, the doc samples, rustdoc's
#                                     intra-doc links for `noeta-ext-abi` (see the scope note at
#                                     that step — it is that crate only, and why), the JIT's
#                                     own differential, and the `#[ignore]`d real-socket end-to-end
#                                     suites (hot reload, LiveView, drain).
#                                     This is the set a merge to `main` must clear.
#   perf          (tier 2, default)  — the instructions-retired ratchet: startup, the interpreter
#                                     dispatch loop, a map workload, and whether tier 1 still
#                                     compiles what it used to. Tier 2 rather than tier 3 because
#                                     the regression it exists to catch (2x startup, 7-11%
#                                     interpreter, ~1,800 commits, nothing noticed) would have
#                                     survived a gate that only ran before a release tag.
#   wasm/aot/miri/editors (tier 3)   — portability, the linked `--native` differential, `unsafe`
#                                     soundness, and the editor grammars. Real gates, but they need
#                                     wasmtime/a C toolchain/nightly+miri/npm and they dominate the
#                                     runtime (the AOT one links a binary per corpus program), so
#                                     they are opt-in per merge, not per commit. `--full` is what
#                                     you run before a release tag.

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
# `noeta-pm` declares no `default` feature and gates every crypto/trust module, so the workspace run
# above builds it BARE — 57 of its 250 tests never executed here or in ci.yml (audit row 4b). Among
# them: the `test_data/wire` fixture pin that keeps this repo byte-compatible with the noeta-registry
# Worker, the advisory-feed and transparency-log chain verification, and the attestation goldens.
# `--all-features` rather than a feature list on purpose: a list is a second place to forget when a
# feature is added, which is the failure this audit is about.
step 2 test "cargo test -p noeta-pm --all-features (registry wire fixtures + trust chain)" -- \
    "${CARGO[@]}" test -p noeta-pm --all-features --locked
# ...and the half no single-repo test run can see. The wire fixtures are copied verbatim into the
# noeta-registry repo, so the two suites can be green on stale copies of two different protocols;
# `--check` compares this checkout against the registry checkout when one is present (it names the
# drifting file and exits non-zero), and reports "checked this repo only" when there is not.
step 1 test "wire fixtures in sync with the registry repo" -- \
    "$ROOT/scripts/sync-wire-fixtures.sh" --check
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

# --- docs: the wiki describes the present, and --help speaks to users --------------------------
#
# The step above proves the samples still compile; nothing checked the prose around them. Docs
# drift into changelog one sentence at a time ("this page used to say…", "the old advice is
# retired"), and internal milestone labels leak into user-facing `--help`. Cheap enough to run in
# the quick tier: it reads files and matches strings, no build of its own beyond the test binary.
step 1 docs "docs style (no history, no milestone vocabulary)" -- \
    "${CARGO[@]}" test -p noeta-cli --test docs_style --no-default-features --locked

# --- docs: rustdoc's own links, for the crate whose product IS its documentation ---------------
#
# The step above runs the ```noeta samples in docs/*.md. Nothing ran rustdoc, so a broken
# `[`Item`]` link was a *warning* on a `cargo doc` that still exited 0 — invisible. There were 28
# of them in `noeta-ext-abi` when this step was written, ten to a `Registry::validate` that is a
# private free function and never was a method, and one whose doc line had drifted off the function
# it described onto the const below it.
#
# SCOPE — `noeta-ext-abi` ONLY, and that is a deliberate cut, not an oversight:
#   * COVERED: the whole public and private doc surface of `crates/noeta-ext-abi`. This crate's
#     entire product is a documented contract for third-party extension authors, who read it as
#     rustdoc; a dead link there is the first thing an outside reader hits, and there is no other
#     gate on it.
#   * NOT COVERED: every other crate. Measured before scoping — `-D broken_intra_doc_links` over
#     `cargo doc --workspace --no-deps --keep-going` reports **137** errors across 22 crates
#     (`noeta-pm` 19, `noeta-check` 18, `noeta-ide` 12, …), which is a cleanup arc, not a step.
#     Widening this to the workspace means doing that arc first; until then a green gate says
#     nothing about any crate but this one, and the name says so.
#
# Both link lints are denied, because both are the same failure to a reader: `broken_intra_doc_links`
# is a link to nothing, `private_intra_doc_links` is a link the public page renders as plain text.
# `--no-deps` so the verdict is about this crate's own prose. RUSTDOCFLAGS is passed through `env`
# rather than exported, so it cannot leak into any other step's cargo invocation.
step 2 docs "rustdoc intra-doc links (noeta-ext-abi)" -- \
    env RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links" \
    "${CARGO[@]}" doc -p noeta-ext-abi --no-deps --locked

# --- jit: the tier-0/tier-1 differential + leak-under-JIT --------------------------------------
step 2 jit "JIT unit tests (noeta-vm)" -- \
    "${CARGO[@]}" test -p noeta-vm --features jit --locked
step 2 jit "JIT differential + leak corpus gate" -- \
    "${CARGO[@]}" test -p noeta-conformance --features jit --locked
step 2 jit "JIT differential (coverage summary)" -- \
    "${CARGO[@]}" run -p noeta-conformance --features jit --locked -- --jit-differential
step 2 jit "JIT differential (cancel-poll arm)" -- \
    "${CARGO[@]}" run -p noeta-conformance --features jit --locked -- --jit-differential --cancel-poll
# The AOT arm: `noeta build --native` emits a third shape of native code (inline caches off, null
# call sites, no poll). Three comments in the tree claimed it was "proven corpus-wide by the
# NOETA_JIT_AOT oracle" — an environment variable that ran in no gate and no workflow. It is now a
# run option, so this arm is the same corpus at the same cost as the two above, with no linker.
step 2 jit "JIT differential (AOT-bodies arm)" -- \
    "${CARGO[@]}" run -p noeta-conformance --features jit --locked -- --jit-differential --aot-bodies
step 2 jit "JIT-enabled CLI (integration + doc samples)" -- \
    "${CARGO[@]}" test -p noeta-cli --locked
# The `#[ignore]`d real-socket suites — hot reload, LiveView, graceful drain. The step above builds
# them and runs none of them (that is what `#[ignore]` means), so this runs them explicitly, through
# the same script ci.yml's `jit` job calls. Tier 2, not tier 3: measured at 5s warm over 62 runs,
# it is the cheapest step in the merge gate, and it is the only one that watches a swap land in a
# server that is actually serving. See scripts/hot-e2e.sh for what it covers and what it asserts.
step 2 jit "hot-reload e2e (#[ignore]d real-socket suites)" -- \
    env "NOETA_GATE_TOOLCHAIN=$TC" bash "$ROOT/scripts/hot-e2e.sh"

# --- perf: the instructions-retired ratchet ----------------------------------------------------
#
# The gap this closes: ~1,800 commits landed a 2x startup regression and a 7-11% interpreter
# regression, and NOTHING in this tree noticed — because everything that could have noticed was
# wall-clock, and wall-clock cannot gate on a box that routinely carries several concurrent agent
# builds (load 6-13 is normal; a whole field run of wall-clock benchmarks inflates ~2x together).
# Instructions retired is a different instrument: the deterministic rows repeat to 0.001-0.08%
# under exactly the load that moves wall-clock by 2x. See scripts/perf-ratchet.sh for the measured
# variance behind every tolerance, for the tier-1 decline check, and for how to re-baseline.
#
# Tier 2, because a gate that only runs before a release tag would not have caught this one either.
# Its cost is the release build below, which is a SECOND full link of the CLI (codegen-units=1 +
# thin LTO) — named as its own step so the minutes land on the line that spends them rather than
# on the ratchet, which itself takes seconds.
#
# The SKIP path is not decorative. `perf` is not everywhere, `perf_event_paranoid` can forbid
# counting, a VM may expose no PMU, and instruction counts are not comparable across machines — so
# `--preflight` answers "can this box produce a number that means anything" and a no becomes a
# loud SKIP naming the reason and its fix. A SKIP is never a PASS here (and under
# NOETA_GATE_REQUIRE_TOOLS it is a FAIL), which is the rule this repo already had to learn twice:
# once from a wasm oracle that sat red while reporting SKIP, and once from `noeta bench`'s own
# regression gate reporting success on a run that measured nothing (c619853bd).
#
# NOETA_PERF_TOOLCHAIN is not optional here: the ratchet folds `rustc -V` into the baseline's
# machine fingerprint (two rustc patch releases inline differently, which on this codebase is
# worth percent, not noise), and it has to be told which toolchain built the binary it is handed.
# Passing $TC keeps the measured build and the recorded baseline describing the same compiler.
PERF_PREFLIGHT_LOG="$LOGDIR/00-perf-preflight.log"
if env "NOETA_PERF_TOOLCHAIN=$TC" bash "$ROOT/scripts/perf-ratchet.sh" --preflight \
    > "$PERF_PREFLIGHT_LOG" 2>&1; then
    step 2 perf "build the noeta CLI, release (the ratchet's subject)" -- \
        "${CARGO[@]}" build --release -p noeta-cli --locked
    step 2 perf "instructions-retired ratchet (startup, interpreter, tier-1 decline)" -- \
        env "NOETA_PERF_TOOLCHAIN=$TC" bash "$ROOT/scripts/perf-ratchet.sh"
else
    PERF_REASON="$(sed -n 's/^perf-ratchet: CANNOT MEASURE — //p' "$PERF_PREFLIGHT_LOG" | head -1)"
    PERF_FIX="$(sed -n 's/^perf-ratchet: fix: //p' "$PERF_PREFLIGHT_LOG" | head -1)"
    skip 2 perf "instructions-retired ratchet (startup, interpreter, tier-1 decline)" \
        "${PERF_REASON:-perf-ratchet.sh --preflight failed; see $PERF_PREFLIGHT_LOG}" \
        "${PERF_FIX:-bash scripts/perf-ratchet.sh --preflight   # for the full reason}"
fi

# --- aot: the LINKED `--native` differential ---------------------------------------------------
#
# The in-process arm above proves the AOT *codegen*. This proves the artifact: for every corpus
# program, `cc`-link the AOT object against the real `libnoeta_aot.a`, staple the bundle on, run the
# binary, and compare it against `noeta run` over the same module — stdout, stderr and exit code.
# That is the only thing watching the linker, the dispatch table's layout (an AOT-only soundness bug
# once lived exactly there: `0f9752d4c`) and the AOT run tail.
#
# Tier 3, not 2: a link per program is minutes, not seconds — measured and recorded in the oracle's
# own summary line, which prints the wall-clock split every run. Its one prerequisite is a C
# toolchain, named with the command that installs it rather than skipped in silence.
CC_BIN="${NOETA_CC:-cc}"
if have "$CC_BIN"; then
    # The archive is its own step so a failure to BUILD it does not read as an oracle failure. The
    # oracle re-runs the same `cargo rustc` (a no-op once built) to read the `native-static-libs`
    # line, which is how `noeta build --native` itself learns the link line.
    step 3 aot "build the AOT runtime archive (libnoeta_aot.a)" -- \
        "${CARGO[@]}" rustc -p noeta-aot-runtime --locked -- --print native-static-libs
    # The truth side is the shipped `noeta` binary, so it must exist before the oracle runs.
    step 3 aot "build the noeta CLI (the AOT oracle's truth side)" -- \
        "${CARGO[@]}" build -p noeta-cli --locked
    step 3 aot "AOT differential (linked --native artifacts vs noeta run)" -- \
        "${CARGO[@]}" run -p noeta-conformance --features jit --locked -- --aot-differential
else
    skip 3 aot "AOT differential (linked --native artifacts vs noeta run)" \
        "no C toolchain: \`$CC_BIN\` is not on PATH" \
        "sudo apt install build-essential   # or set NOETA_CC=/path/to/cc"
fi

# --- wasm: the portability invariant + the wasm/browser/serve oracles --------------------------
#
# Two prerequisites, and BOTH have silently disarmed this group before, so each reports which one
# is missing and the exact command that fixes it:
#
#   * the rustup target, FOR THE GATING TOOLCHAIN. `rustup target add wasm32-wasip1` adds it to the
#     default toolchain; this gate pins `+1.97.0`. A target added to the wrong toolchain looks
#     installed (it is, for `stable`) and does nothing here.
#   * wasmtime, which its own installer puts in `~/.wasmtime/bin` and exports from a shell rc file
#     — invisible to a non-interactive run. See `find_wasmtime`.
target_hint() { # <target> -> the command that installs it for the gating toolchain
    if [[ "$TC" == "default" ]]; then
        printf 'rustup target add %s' "$1"
    else
        printf 'rustup target add --toolchain %s %s' "$TC" "$1"
    fi
}
target_reason() { # <target> -> why the step cannot run
    if [[ "$TC" == "default" ]]; then
        printf 'target %s not installed' "$1"
    else
        printf 'target %s not installed for the gating toolchain %s (it may be installed for another)' "$1" "$TC"
    fi
}
WASMTIME="$(find_wasmtime || true)"
WASMTIME_REASON="wasmtime not found on PATH or in the usual install dirs (~/.wasmtime/bin, /usr/local/bin, …)"
WASMTIME_FIX="curl https://wasmtime.dev/install.sh -sSf | bash    # or set NOETA_WASMTIME=/path/to/wasmtime"
if [[ -n "$WASMTIME" ]]; then
    printf '\ngate: wasmtime: %s\n' "$WASMTIME"
fi

if has_target wasm32-wasip1; then
    step 3 wasm "check pipeline crates (wasm32-wasip1)" -- \
        "${CARGO[@]}" check -p noeta-vm -p noeta-stdlib -p noeta-compiler -p noeta-lexer \
        -p noeta-parser -p noeta-loader -p noeta-db -p noeta-bundle -p noeta-eval \
        --target wasm32-wasip1 --locked
    step 3 wasm "build the wasm runner (wasm-release)" -- \
        "${CARGO[@]}" build -p noeta-wasm-runner --target wasm32-wasip1 --profile wasm-release --locked
else
    skip 3 wasm "check pipeline crates (wasm32-wasip1)" \
        "$(target_reason wasm32-wasip1)" "$(target_hint wasm32-wasip1)"
    skip 3 wasm "build the wasm runner (wasm-release)" \
        "$(target_reason wasm32-wasip1)" "$(target_hint wasm32-wasip1)"
fi

# The ship-safety oracle for the wasm target. Name whichever prerequisite is actually missing —
# "wasmtime not on PATH" was printed even when wasmtime was present and the rustup target was the
# real gap, which sent people looking in the wrong place.
if [[ -n "$WASMTIME" ]] && has_target wasm32-wasip1; then
    step 3 wasm "wasm differential oracle (runner vs native VM)" -- \
        env "NOETA_WASMTIME=$WASMTIME" "${CARGO[@]}" run -p noeta-conformance --locked -- --wasm-differential
elif [[ -z "$WASMTIME" ]] && ! has_target wasm32-wasip1; then
    skip 3 wasm "wasm differential oracle (runner vs native VM)" \
        "$WASMTIME_REASON; and $(target_reason wasm32-wasip1)" \
        "$(target_hint wasm32-wasip1)  &&  $WASMTIME_FIX"
elif [[ -z "$WASMTIME" ]]; then
    skip 3 wasm "wasm differential oracle (runner vs native VM)" "$WASMTIME_REASON" "$WASMTIME_FIX"
else
    skip 3 wasm "wasm differential oracle (runner vs native VM)" \
        "$(target_reason wasm32-wasip1)" "$(target_hint wasm32-wasip1)"
fi

if has_target wasm32-unknown-unknown; then
    step 3 wasm "build the playground engine (wasm32-unknown-unknown)" -- \
        "${CARGO[@]}" build -p noeta-playground --target wasm32-unknown-unknown --profile wasm-release --locked
    if have node; then
        step 3 wasm "browser-engine smoke (node over the raw ABI)" -- \
            node crates/noeta-playground/tests/browser_smoke.mjs \
            "${CARGO_TARGET_DIR:-$ROOT/target}/wasm32-unknown-unknown/wasm-release/noeta_playground.wasm"
    else
        skip 3 wasm "browser-engine smoke (node over the raw ABI)" "node not on PATH" \
            "install Node.js (https://nodejs.org)"
    fi
else
    skip 3 wasm "build the playground engine (wasm32-unknown-unknown)" \
        "$(target_reason wasm32-unknown-unknown)" "$(target_hint wasm32-unknown-unknown)"
    skip 3 wasm "browser-engine smoke (node over the raw ABI)" \
        "$(target_reason wasm32-unknown-unknown)" "$(target_hint wasm32-unknown-unknown)"
fi

if [[ -n "$WASMTIME" ]] && has_target wasm32-wasip2; then
    step 3 wasm "wasi:http serve e2e (component under wasmtime serve)" -- \
        env "NOETA_WASMTIME=$WASMTIME" "PATH=$(dirname "$WASMTIME"):$PATH" \
        bash crates/noeta-wasm-serve/tests/e2e.sh
elif [[ -z "$WASMTIME" ]] && ! has_target wasm32-wasip2; then
    skip 3 wasm "wasi:http serve e2e (component under wasmtime serve)" \
        "$WASMTIME_REASON; and $(target_reason wasm32-wasip2)" \
        "$(target_hint wasm32-wasip2)  &&  $WASMTIME_FIX"
elif [[ -z "$WASMTIME" ]]; then
    skip 3 wasm "wasi:http serve e2e (component under wasmtime serve)" "$WASMTIME_REASON" "$WASMTIME_FIX"
else
    skip 3 wasm "wasi:http serve e2e (component under wasmtime serve)" \
        "$(target_reason wasm32-wasip2)" "$(target_hint wasm32-wasip2)"
fi

# --- miri: the one place `unsafe` lives -------------------------------------------------------
if rustup component list --toolchain nightly --installed 2>/dev/null | grep -q '^miri'; then
    step 3 miri "cargo miri test (noeta-value, noeta-gc)" -- \
        env PROPTEST_CASES=16 MIRIFLAGS=-Zmiri-disable-isolation \
        cargo +nightly miri test -p noeta-value -p noeta-gc --locked
else
    skip 3 miri "cargo miri test (noeta-value, noeta-gc)" "nightly miri not installed" \
        "rustup +nightly component add miri"
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
    skip 3 editors "editor tooling (tree-sitter + VS Code)" "npm not on PATH" \
        "install Node.js (https://nodejs.org)"
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

# A run that executed NOTHING is not a pass. `--only wasm` against the default tier matched no step
# (the wasm steps live in --full), and this printed `GATE PASSED`, exit 0 — a green light for a run
# that gated nothing, which is the exact failure mode the rest of this script is built to avoid.
# Found by using it: I typed that command to verify the wasm tier and was told everything was fine.
#
# `--only` now overrides the tier (see `selected`), so that particular command works. This stays,
# because the class does not: a typo'd substring still matches nothing, and a run that gated nothing
# must never read as a run that passed.
if ((n_pass + n_fail + n_skip == 0)); then
    echo
    if [[ -n "$ONLY" ]]; then
        printf ' \033[31mNO STEP MATCHED --only %s.\033[0m\n' "$ONLY"
        echo '  Nothing ran, so nothing is known. `--list` prints this tier'"'"'s steps; a step may'
        echo '  live in a wider tier (the wasm and miri steps are --full).'
    else
        printf ' \033[31mNO STEPS RAN.\033[0m The plan for this tier is empty — that is a bug in the gate.\n'
    fi
    echo
    echo ' GATE FAILED — it tested nothing.'
    exit 1
fi

if ((n_fail)); then
    echo
    echo ' Reproduce the failures:'
    for i in "${!S_NAME[@]}"; do
        [[ "${S_STATUS[$i]}" == FAIL ]] || continue
        if [[ -n "${S_LOG[$i]}" ]]; then
            printf '   %s\n     log: %s\n' "${S_CMD[$i]}" "${S_LOG[$i]}"
        else
            # A missing prerequisite under REQUIRE_TOOLS: there is no log, the fix is the command.
            printf '   %s\n     %s\n' "${S_NAME[$i]}" "${S_NOTE[$i]}"
            [[ -n "${S_CMD[$i]}" ]] && printf '     fix: %s\n' "${S_CMD[$i]}"
        fi
    done
    echo
    echo ' GATE FAILED — do not merge.'
    exit 1
fi

if ((n_skip)); then
    echo
    printf ' \033[33mWARNING: %d step(s) did NOT RUN. A SKIP is not a PASS.\033[0m\n' "$n_skip"
    echo '  These gates tested NOTHING in this run:'
    for i in "${!S_NAME[@]}"; do
        [[ "${S_STATUS[$i]}" == SKIP ]] || continue
        printf '   [%-7s] %s\n     %s\n' "${S_GROUP[$i]}" "${S_NAME[$i]}" "${S_NOTE[$i]}"
        [[ -n "${S_CMD[$i]}" ]] && printf '     fix: %s\n' "${S_CMD[$i]}"
    done
    echo
    echo '  Install the prerequisites above, or accept that CI is the first thing to execute these.'
    echo '  To make a missing prerequisite a hard failure instead: NOETA_GATE_REQUIRE_TOOLS=1'
    echo '  (already the default when $CI is set).'
fi

echo
echo ' GATE PASSED.'
exit 0
