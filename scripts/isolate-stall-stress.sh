#!/usr/bin/env bash
# Stress the real-isolate spawn window against the stall registry's all-parties-blocked check.
#
# A parallel scheduler that parks while the global `active` count still misses a live worker sees
# `parked == active` and latches a deadlock, so a correct program aborts with E0010. The window that
# can produce that undercount is the parent thread's spawn path: the worker's registry slot is added
# by the parent, and the worker's own thread is live from the moment it starts. This harness drives
# the window under enough CPU oversubscription to keep the parent off-CPU across it, and counts how
# many runs of a program that cannot deadlock nevertheless abort.
#
# The deterministic form of the same check is
# `run_real_isolate_spawn_window_is_not_a_false_deadlock` in crates/noeta-cli/tests/cli/isolates.rs,
# which pins the parent inside the window with NOETA_ISOLATE_SPAWN_DELAY_MS instead of racing for it.
# That test runs in the ordinary suite. This script is a manual instrument: nothing runs it for you,
# and it is here to measure a rate rather than to gate one.
#
# Usage: scripts/isolate-stall-stress.sh [--runs N] [--spawns N] [--load N] [--timeout SECS] [--bin PATH]
set -u

runs=200
spawns=200
load=$(( $(nproc) * 3 ))
timeout_secs=30
bin=""

while [ $# -gt 0 ]; do
  case "$1" in
    --runs) runs="$2"; shift 2 ;;
    --spawns) spawns="$2"; shift 2 ;;
    --load) load="$2"; shift 2 ;;
    --timeout) timeout_secs="$2"; shift 2 ;;
    --bin) bin="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "$bin" ]; then
  bin="${CARGO_TARGET_DIR:-$root/target}/debug/noeta"
fi
if [ ! -x "$bin" ]; then
  echo "no noeta binary at $bin (build it, or pass --bin)" >&2
  exit 2
fi

# A branch-keyed scratch directory: several sessions work this repository at once, and a fixed name
# under a shared temp root would have them truncate each other's fixture.
branch="$(git -C "$root" rev-parse --abbrev-ref HEAD 2>/dev/null | tr '/' '-')"
work="${TMPDIR:-/tmp}/noeta-stall-stress-${branch:-detached}-$$"
mkdir -p "$work"
trap 'rm -rf "$work"' EXIT

# The worker's first scheduler step is a blocking `recv`, so it parks as early as a real isolate can
# — the earliest point at which it can consume a registry slot it has not been given. The parent
# feeds and closes the channel straight after, so the program has no way to deadlock: every E0010
# from this file is a false positive.
cat > "$work/main.noe" <<EOF
async fn waiter(rx: Receiver<int>): int {
  r = rx.recv().await
  return match r { some(x) => x, none => 0 }
}

async fn run(): int {
  mut total = 0
  mut i = 0
  while i < $spawns {
    (tx, rx) = channel::<int>(1)
    mut got = 0
    concurrent {
      h = isolate waiter(rx)
      tx.send(1).await
      tx.close()
      got = h.await
    }
    total = total + got
    i = i + 1
  }
  return total
}

echo run().await
EOF

load_pids=()
start_load() {
  local i
  for (( i = 0; i < load; i++ )); do
    # A builtin-only spin loop: runnable work with no forking and no disk traffic.
    bash -c 'while :; do :; done' &
    load_pids+=("$!")
  done
}
stop_load() {
  local pid
  for pid in "${load_pids[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
  done
  for pid in "${load_pids[@]:-}"; do
    [ -n "$pid" ] && wait "$pid" 2>/dev/null
  done
  load_pids=()
}
trap 'stop_load; rm -rf "$work"' EXIT

echo "binary:   $bin"
echo "runs:     $runs   spawns/run: $spawns   load threads: $load   timeout: ${timeout_secs}s"
[ "$load" -gt 0 ] && start_load
sleep 1

false_deadlock=0
timedout=0
other=0
ok=0
for (( r = 1; r <= runs; r++ )); do
  out="$work/run-$r.txt"
  timeout "$timeout_secs" "$bin" run "$work/main.noe" > "$out" 2>&1
  status=$?
  if [ "$status" -eq 0 ] && [ "$(cat "$out")" = "$spawns" ]; then
    ok=$(( ok + 1 ))
  elif [ "$status" -eq 124 ]; then
    timedout=$(( timedout + 1 ))
    echo "run $r: TIMEOUT"
  elif grep -q 'E0010' "$out"; then
    false_deadlock=$(( false_deadlock + 1 ))
    echo "run $r: FALSE DEADLOCK"
    sed -n '1,6p' "$out"
  else
    other=$(( other + 1 ))
    echo "run $r: unexpected (exit $status)"
    sed -n '1,6p' "$out"
  fi
  rm -f "$out"
done

stop_load
echo
echo "ok:              $ok / $runs"
echo "false deadlocks: $false_deadlock / $runs   ($(( false_deadlock * spawns )) of $(( runs * spawns )) spawn windows reached, at most)"
echo "timeouts:        $timedout / $runs"
echo "other failures:  $other / $runs"
[ "$false_deadlock" -eq 0 ] && [ "$timedout" -eq 0 ] && [ "$other" -eq 0 ]
