#!/usr/bin/env python3
"""Kernel-methods K5 gate: METHOD form vs MODULE form of the same bulk kernels, one binary.

  python3 run.py [binary]     (default ../../../target/release/noeta)

Pinned (taskset core 2), interleaved ABAB…, median of 7. The method form adds only the baked
call-site route (NameId resolves + the receiver-seeded ctx entry), so the gate is parity.
"""

import os
import shutil
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RUNS = 7

PAIRS = [
    ("../pm-native/v_add_row.noe", "k_add_row_method.noe"),
    ("../pm-native/v_dot_col.noe", "k_dot_col_method.noe"),
]


def pin_prefix() -> list:
    return ["taskset", "-c", "2"] if shutil.which("taskset") else []


def time_run(binary: str, fixture: str) -> float:
    path = os.path.join(HERE, fixture)
    t0 = time.perf_counter()
    proc = subprocess.run(
        pin_prefix() + [binary, "run", path], capture_output=True, text=True, check=False
    )
    wall = (time.perf_counter() - t0) * 1000.0
    if proc.returncode != 0:
        sys.exit(f"{fixture} FAILED rc={proc.returncode}: {proc.stderr.strip()[:300]}")
    return wall


def main() -> None:
    binary = (
        sys.argv[1]
        if len(sys.argv) > 1
        else os.path.join(HERE, "..", "..", "..", "target", "release", "noeta")
    )
    for module_form, method_form in PAIRS:
        time_run(binary, module_form)
        time_run(binary, method_form)  # warmup: compile + startup cache
        ta, tb = [], []
        for _ in range(RUNS):
            ta.append(time_run(binary, module_form))
            tb.append(time_run(binary, method_form))
        ma, mb = statistics.median(ta), statistics.median(tb)
        delta = (mb - ma) / ma * 100.0
        print(
            f"{os.path.basename(module_form):<20} module {ma:7.1f} ms   "
            f"method {mb:7.1f} ms   {delta:+5.1f}%"
        )


if __name__ == "__main__":
    main()
