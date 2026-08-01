#!/usr/bin/env bash
# The real-socket end-to-end suites: hot reload, LiveView, and graceful drain, driven through the
# shipped `noeta` binary. They stay `#[ignore]`d and are run from here, by CI and by the merge gate.
#
# Why these five, and why they need a step of their own: they are the only tests that exercise a
# hot swap the way a user meets one — a real `noeta serve --watch` process, a real listening socket,
# a real file edit, a real websocket. The compile-side gates (the corpus swap differential, the
# lowerer census, the determinism check) prove the swapped bytes are right; nothing but these proves
# the running server actually serves them. That gap is not hypothetical: `hot_serve` and `hot_live`
# once sat FAILING on `main` for weeks, because `#[ignore]` meant nothing ran them until someone
# typed `-- --ignored` by hand.
#
#   scripts/hot-e2e.sh                      the shipped shape (noeta-cli default features = jit)
#   scripts/hot-e2e.sh --no-default-features   the Cranelift-free CLI
#
# Environment:
#   NOETA_GATE_TOOLCHAIN   when set to something other than `default`, gate with `cargo +<tc>`.
#                          Unset means plain `cargo` — which is what CI wants, having already
#                          pinned the toolchain with `dtolnay/rust-toolchain@1.97.0`.
#   CARGO_TARGET_DIR       respected, not set — but note that a HIDDEN target dir (the per-agent
#                          `~/.cargo-targets/...` convention) changes where fixtures are rooted:
#                          `noeta-test-temp` falls back to the system temp dir rather than root
#                          fixtures under a dot-directory, because `noeta serve --watch` ignores
#                          every path below one and these suites would see no file events at all.
#
# No output of a gated command is ever piped: each suite's status is read directly from `$?`, and
# its output is replayed from a file afterwards. Reading a pipeline's status instead of the
# command's is how this repository last shipped a check that reported a false pass.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

# suite:expected-passing-tests.
#
# The count is half the gate. `--ignored` selects only ignored tests, and libtest exits 0 when a
# filter matches nothing — so a renamed, deleted, or accidentally un-`#[ignore]`d test would turn
# this step into a green no-op that still reads as coverage, which is worse than not having it.
# Asserting the number that ran makes that failure loud. Bump the number here when you add a test.
SUITES=(
    "hot_serve:2"       # a body edit swaps in and the signal counter survives it — bare file, and
                        # inside a package, where the handler is qualified (that one shipped broken)
    "hot_live:1"        # the L3 showcase: reload frame, preserved state, error overlay on red check
    "parallel_hot:2"    # `--parallel 3`: one edit must reach EVERY worker isolate; plus the
                        # audit-10 equality — the fleet and the single worker are ONE hot install,
                        # so an idle swap must reach both on the first request after it
    "live_serve:1"      # LiveView over a real RFC 6455 socket: snapshot, patches, second session
    "graceful_drain:1"  # SIGINT mid-request drains it, then the listener closes
    # The four siblings this list was first scoped to leave out. It was scoped to the hot-swap arc;
    # the class is "everything `#[ignore]`d because it needs a real port or a real child process",
    # and a sibling left out of the list is a sibling nothing runs. Added on the measurement the
    # note here asked for: 85 consecutive runs of all nine suites, 765 suite-runs, zero failures —
    # at ambient load, beside a lean-CLI build, beside a release build, with unrestricted intra-suite
    # threads, and pinned to two cores. All four cost under 3s of test time.
    "serve:1"           # plain routing over a loopback socket, plus the empty-probe regression
    "parallel_serve:2"  # `--parallel 4`: shared listener, concurrent slow requests, SIGINT drains all;
                        # plus the worker run tail — an aborting worker renders its diagnostic AND its stack
    "live_stream:3"     # SSE both directions, including a body split mid-frame and mid-CRLF
    "impact_watch:2"    # `noeta test --watch` impact filtering, single-file and across modules
)

# Nothing `#[ignore]`d in `noeta-cli` is left off this list by accident: `tests/cli/automation.rs`
# is a census that fails the build when an ignored test is named by neither this script nor an
# explicit, written exemption — and it checks the counts above against the tree, so the "bump the
# number" instruction is itself enforced rather than remembered.
TC="${NOETA_GATE_TOOLCHAIN:-}"
if [[ -n "$TC" && "$TC" != "default" ]]; then
    CARGO=(cargo "+$TC")
else
    CARGO=(cargo)
fi

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

failed=0
declare -a SUMMARY=()

for entry in "${SUITES[@]}"; do
    suite="${entry%%:*}"
    want="${entry##*:}"
    log="$OUT/$suite.log"

    printf '\n== hot-e2e: %s (expecting %s test(s))\n' "$suite" "$want"
    t0=$SECONDS
    # Not piped: `$?` below is this command's own status.
    "${CARGO[@]}" test -p noeta-cli --locked "$@" --test "$suite" -- --ignored > "$log" 2>&1
    rc=$?
    secs=$((SECONDS - t0))
    cat "$log"

    # Sum the `test result: ok. N passed; ...` lines (one per test binary).
    ran=$(awk '/^test result: ok\./ { for (i = 1; i <= NF; i++) if ($(i + 1) ~ /^passed/) { gsub(/[^0-9]/, "", $i); n += $i } } END { print n + 0 }' "$log")

    if ((rc != 0)); then
        SUMMARY+=("FAIL  ${secs}s  $suite — exit $rc")
        failed=$((failed + 1))
    elif [[ "$ran" != "$want" ]]; then
        SUMMARY+=("FAIL  ${secs}s  $suite — $ran test(s) ran, expected $want (a test was renamed, removed, or un-ignored)")
        failed=$((failed + 1))
    else
        SUMMARY+=("PASS  ${secs}s  $suite — $ran test(s)")
    fi
done

echo
echo "---- hot-e2e summary ----"
printf ' %s\n' "${SUMMARY[@]}"

if ((failed)); then
    echo
    echo "hot-e2e FAILED ($failed of ${#SUITES[@]} suites)."
    exit 1
fi

echo
echo "hot-e2e passed (${#SUITES[@]} suites)."
exit 0
