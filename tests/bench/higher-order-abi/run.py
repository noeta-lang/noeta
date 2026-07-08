#!/usr/bin/env python3
"""Higher-order-ABI bench runner (H-BENCH): the regression gate for the Builtin-family migration.

Times each fixture (median of R runs) on the real path (`noeta run`, release), plus a serve
loopback-throughput case (`noeta serve` + N sequential HTTP requests). Two modes:

  python3 run.py [binary]                    capture (one binary; default ../../target/release/noeta)
  python3 run.py --compare A B               pinned interleaved A/B — the protocol for gating a
                                             migration phase (A = baseline, B = candidate)

Interleaving alternates A and B per run (ABAB...), so drift (thermal, cache, background load)
hits both sides equally. Run fixtures are pinned to one core via taskset when available; the
serve case is never pinned (server + client need concurrency).
"""

import os
import shutil
import socket
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RUNS = 7
SERVE_PORT = 18734
SERVE_REQUESTS = 500

RUN_FIXTURES = [
    "r_get_hot.noe",
    "r_set_flush.noe",
    "r_computed_memo.noe",
    "r_effect_fanout.noe",
    "t_map_bounded.noe",
    "t_all.noe",
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


def serve_throughput(binary: str) -> float:
    """Spawn `noeta serve`, drive SERVE_REQUESTS sequential GETs, return requests/second."""
    app = os.path.join(HERE, "serve_app.noe")
    server = subprocess.Popen(
        [binary, "serve", app, "--port", str(SERVE_PORT)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        # Readiness: poll-connect until the listener is up.
        deadline = time.time() + 10.0
        while True:
            try:
                socket.create_connection(("127.0.0.1", SERVE_PORT), timeout=0.2).close()
                break
            except OSError:
                if time.time() > deadline:
                    sys.exit(f"serve_app never came up ({binary})")
                time.sleep(0.05)
        import http.client

        t0 = time.perf_counter()
        for _ in range(SERVE_REQUESTS):
            conn = http.client.HTTPConnection("127.0.0.1", SERVE_PORT, timeout=5)
            conn.request("GET", "/")
            resp = conn.getresponse()
            resp.read()
            conn.close()
            if resp.status != 200:
                sys.exit(f"serve_app returned {resp.status}")
        elapsed = time.perf_counter() - t0
        return SERVE_REQUESTS / elapsed
    finally:
        server.terminate()
        server.wait(timeout=5)


def capture(binary: str) -> None:
    print(f"binary: {binary}\nruns per fixture: {RUNS}\n")
    for fixture in RUN_FIXTURES:
        walls = [time_run(binary, fixture) for _ in range(RUNS)]
        print(f"{fixture:24s} wall median {statistics.median(walls):8.1f} ms (min {min(walls):7.1f})")
    rps = serve_throughput(binary)
    print(f"{'serve_app.noe':24s} {rps:8.0f} req/s ({SERVE_REQUESTS} sequential loopback GETs)")


def compare(a: str, b: str) -> None:
    print(f"A (baseline):  {a}\nB (candidate): {b}\nruns per side:  {RUNS}, interleaved\n")
    for fixture in RUN_FIXTURES:
        wa, wb = [], []
        for _ in range(RUNS):  # ABAB... so drift hits both sides
            wa.append(time_run(a, fixture))
            wb.append(time_run(b, fixture))
        ma, mb = statistics.median(wa), statistics.median(wb)
        delta = (mb - ma) / ma * 100.0
        print(f"{fixture:24s} A {ma:8.1f} ms   B {mb:8.1f} ms   {delta:+6.1f}%")
    ra, rb = [], []
    for _ in range(3):  # serve interleaved too, fewer rounds (each is 500 requests)
        ra.append(serve_throughput(a))
        rb.append(serve_throughput(b))
    ma, mb = statistics.median(ra), statistics.median(rb)
    print(f"{'serve_app.noe':24s} A {ma:8.0f} req/s B {mb:8.0f} req/s {(mb - ma) / ma * 100.0:+6.1f}%")


def main() -> None:
    args = sys.argv[1:]
    if args and args[0] == "--compare":
        if len(args) != 3:
            sys.exit("usage: run.py --compare <baseline-binary> <candidate-binary>")
        compare(os.path.abspath(args[1]), os.path.abspath(args[2]))
        return
    binary = args[0] if args else os.path.join(HERE, "../../../target/release/noeta")
    capture(os.path.abspath(binary))


if __name__ == "__main__":
    main()
