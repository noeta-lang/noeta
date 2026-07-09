#!/usr/bin/env python3
"""N3.4 bench runner: the regression gate for the vec bulk-kernel migration onto the raw-buffer
ctx seam (package-manager Phase 3). Same protocol as the higher-order-abi runner:

  python3 run.py [binary]                    capture (one binary; default ../../../target/release/noeta)
  python3 run.py --compare A B               pinned interleaved A/B (A = baseline, B = candidate)

Interleaving alternates A and B per run (ABAB...), so drift (thermal, cache, background load)
hits both sides equally; fixtures are pinned to one core via taskset when available.
"""

import os
import shutil
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RUNS = 7

FIXTURES = [
    "v_add_row.noe",
    "v_dot_col.noe",
    "v_scale_row.noe",
    "v_add_boxed.noe",
]


def pin_prefix() -> list:
    return ["taskset", "-c", "2"] if shutil.which("taskset") else []


def time_run(binary: str, fixture: str) -> float:
    """One `noeta run` of a fixture; returns wall ms. Exits loudly on failure."""
    path = os.path.join(HERE, fixture)
    t0 = time.perf_counter()
    proc = subprocess.run(
        pin_prefix() + [binary, "run", path], capture_output=True, text=True, check=False
    )
    wall = (time.perf_counter() - t0) * 1000.0
    if proc.returncode != 0:
        sys.exit(f"{fixture} FAILED rc={proc.returncode} ({binary}): {proc.stderr.strip()[:300]}")
    return wall


def capture(binary: str) -> None:
    for fixture in FIXTURES:
        times = [time_run(binary, fixture) for _ in range(RUNS)]
        print(f"{fixture:<20} median {statistics.median(times):8.1f} ms  (n={RUNS})")


def compare(a: str, b: str) -> None:
    print(f"A = {a}\nB = {b}\ninterleaved ABAB…, {RUNS} runs each, median\n")
    for fixture in FIXTURES:
        ta, tb = [], []
        # One unrecorded warmup per side: the first run compiles + fills the bytecode cache.
        time_run(a, fixture)
        time_run(b, fixture)
        for _ in range(RUNS):
            ta.append(time_run(a, fixture))
            tb.append(time_run(b, fixture))
        ma, mb = statistics.median(ta), statistics.median(tb)
        delta = (mb - ma) / ma * 100.0
        print(f"{fixture:<20} A {ma:8.1f} ms   B {mb:8.1f} ms   {delta:+6.1f}%")


def main() -> None:
    args = sys.argv[1:]
    if args and args[0] == "--compare":
        if len(args) != 3:
            sys.exit("usage: run.py --compare A B")
        compare(args[1], args[2])
        return
    binary = args[0] if args else os.path.join(HERE, "..", "..", "..", "target", "release", "noeta")
    capture(binary)


if __name__ == "__main__":
    main()
