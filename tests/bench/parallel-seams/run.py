#!/usr/bin/env python3
"""P-PAR S0 runner: times each fixture (median of R runs) and reports child max-RSS.

Usage: python3 run.py [path-to-noeta-binary]
Defaults to ../../target/release/noeta relative to this file. Max-RSS comes from
resource.getrusage(RUSAGE_CHILDREN), so run each fixture in a fresh subprocess-of-this-script
if you need per-fixture RSS isolation — this script forks itself per fixture for exactly that.
"""

import os
import resource
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RUNS = 7

FIXTURES = [
    "fanout_n0.noe",
    "fanout_n1.noe",
    "fanout_n2.noe",
    "fanout_n4.noe",
    "fanout_n8.noe",
    "pingpong_coop.noe",
    "pingpong.noe",
]


def measure_one(binary: str, fixture: str) -> None:
    """Child mode: run one fixture RUNS times, print median wall ms + max child RSS + CPU time."""
    path = os.path.join(HERE, fixture)
    walls = []
    for _ in range(RUNS):
        t0 = time.perf_counter()
        proc = subprocess.run(
            [binary, "run", path], capture_output=True, text=True, check=False
        )
        walls.append((time.perf_counter() - t0) * 1000.0)
        if proc.returncode != 0:
            print(f"{fixture}: FAILED rc={proc.returncode}: {proc.stderr.strip()[:200]}")
            return
    ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu_ms = (ru.ru_utime + ru.ru_stime) / (RUNS) * 1000.0
    print(
        f"{fixture:22s} wall median {statistics.median(walls):8.1f} ms "
        f"(min {min(walls):7.1f})  cpu/run {cpu_ms:8.1f} ms  maxrss {ru.ru_maxrss // 1024:5d} MB"
    )


def main() -> None:
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "../../../target/release/noeta")
    binary = os.path.abspath(binary)
    if os.environ.get("PPAR_CHILD"):
        measure_one(binary, os.environ["PPAR_CHILD"])
        return
    print(f"binary: {binary}\nruns per fixture: {RUNS}\n")
    for fixture in FIXTURES:
        env = dict(os.environ, PPAR_CHILD=fixture)
        subprocess.run([sys.executable, os.path.abspath(__file__), binary], env=env, check=False)


if __name__ == "__main__":
    main()
