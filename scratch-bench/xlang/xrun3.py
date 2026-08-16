#!/usr/bin/env python3
"""Cross-language micro-benchmark, 3 pinned field runs, with an in-session A/B against the
2026-07-17 baseline binaries.

Same method as xrun.py (taskset-pinned, min-of-N after warmup, startup-subtracted) but:
  * raw totals and raw startups are kept per field run, and compute is reconstructed as
    min(total over all field runs) - min(startup over all field runs) — the documented
    method, which per-run subtraction only approximates;
  * the cached 07-17 binaries run in the SAME session, so a regression is measured against
    the old code on today's machine rather than against a two-week-old table.

Wall-clock is what this harness *reports*; it decides nothing. Every claim made here — the tier-1
coverage check at the bottom — is computed from instructions retired, for the reason the whole
workstream measures that way: a sibling agent build inflates a whole wall-clock field ~2x, so a few
percent of wall-clock carries no information at all. `--tier1-only` runs that check by itself,
without the (much longer) wall-clock field.
"""
import subprocess, time, shutil, os, sys, json

import jitstats
from jitstats import DeclineStatus

REPS = int(os.environ.get("BENCH_REPS", "9"))
FIELDS = int(os.environ.get("BENCH_FIELDS", "3"))
# min-of-5 rather than min-of-3 for the instruction counts: tier-1 compiles on a background thread,
# so how many iterations a short benchmark runs interpreted before native entry varies run to run —
# measured at up to 14% on `strcat`, against 0.0x% on a JIT-less binary, which has no such thread.
# The min is the run that reached native soonest, and more reps make it a tighter bound.
ICOUNT_REPS = int(os.environ.get("ICOUNT_REPS", "5"))
SP = os.environ.get("BENCH_BIN_DIR", os.path.expanduser("~/.cache/noeta-bench"))
XL = os.path.dirname(os.path.abspath(__file__))
PIN = ["taskset", "-c", "3"]

def php_jit(f):
    return ["php", "-dopcache.enable_cli=1", "-dopcache.jit_buffer_size=128M",
            "-dopcache.jit=tracing", f + ".php"]

LANGS = [
    ("Noeta-JIT",   lambda f: [os.path.join(SP, "noeta-jit"), "run", f + ".noe"]),
    ("Noeta-int",   lambda f: [os.path.join(SP, "noeta-int"), "run", f + ".noe"]),
    ("JIT@0802",    lambda f: [os.path.join(SP, "noeta-jit-0802"), "run", f + ".noe"]),
    ("int@0802",    lambda f: [os.path.join(SP, "noeta-int-0802"), "run", f + ".noe"]),
    ("PHP",         lambda f: ["php", f + ".php"]),
    ("PHP+JIT",     php_jit),
    ("LuaJIT",      lambda f: ["luajit", f + ".lua"]),
    ("Lua",         lambda f: ["lua", f + ".lua"]),
    ("Python",      lambda f: ["python3", f + ".py"]),
]
BENCHES = ["loop", "fib", "strcat", "assoc", "wordcount"]

def have(argv):
    exe = argv[0]
    return (os.path.isfile(exe) and os.access(exe, os.X_OK)) or shutil.which(exe) is not None

def run_min(argv, reps=REPS):
    argv = PIN + argv
    try:
        subprocess.run(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=180)
    except Exception:
        return None, None
    best = float("inf"); out = None
    for _ in range(reps):
        t0 = time.perf_counter()
        try:
            p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180)
        except Exception:
            return None, None
        dt = (time.perf_counter() - t0) * 1000.0
        if p.returncode != 0:
            return None, "ERR:" + p.stderr.decode()[:200].replace("\n", " ")
        if dt < best:
            best = dt; out = p.stdout.decode().strip()
    return best, out

# --- the tier-1 coverage check ------------------------------------------------------------------
# How close the JIT and interpreter binaries have to be before we stop calling it "the JIT did not
# help much" and start calling it "the JIT did not run".
#
# The number is a fraction of INSTRUCTIONS RETIRED, not of the wall-clock field above, and that is
# the load-bearing part. Wall-clock on this box moves with whatever else is building: a whole field
# inflates ~2x under the load a couple of sibling agents produce, so a few percent of wall-clock
# says nothing about whether native code ran. Measured on the 2026-08-16 field, the wall-clock
# columns put `strcat` 3.0% apart — a tenth of a point from declaring the JIT dead on a row where
# instructions retired have it 1.36x ahead. Instructions retired repeat to 0.001-0.08% under
# exactly that load, which is why `scripts/perf-ratchet.sh` gates on them and why this asks them.
#
# What the two cases look like, which is what makes a threshold choosable at all. Both were
# measured, not assumed — `NOETA_JIT_ABLATE=0x7f` turns the leaf-op set back off, which makes tier 1
# decline the string/map loops for real while leaving the arithmetic ones native:
#
#   * A genuinely non-running tier 1 means both binaries walked the same interpreter over the same
#     program, so the counts agree to several digits: assoc 462.1M vs 457.6M (0.98%), wordcount
#     661.8M vs 656.0M (0.88%), strcat 87.6M vs 85.7M (2.1%, the widest — on the shortest of the
#     five the fixed cost of the JIT build's failed compile attempt is a bigger slice). The residue
#     is the JIT build's tier-1 bookkeeping, and it points the wrong way: the JIT column does
#     slightly MORE work, never less. Pointing both columns at one binary makes it exactly 0.00%.
#   * A real win is nothing like that. The smallest ever measured on this field is 1.25-1.38x on the
#     string/map rows — 20-28% fewer instructions — and loop/fib are 9.6-10.4x, 89-90% fewer.
#
# So the two populations are an order of magnitude apart and the threshold sits in an empty band.
# 5% is chosen from the wide end of the measured "not running" case (2.4x above it) and stays 4x
# under the smallest measured win. A "win" under 5% of instructions would be no reason to run a JIT
# anyway, and it is reported rather than swallowed: the verdict is signed, so a JIT column doing
# MORE work than the interpreter is never an `ok` row no matter how far apart the two are.
TIER1_EQUAL_PCT = 5.0


def icount(argv, reps=ICOUNT_REPS):
    """min-of-N instructions retired for one command, or None if perf could not count.

    Same instrument as `measure.sh` and `scripts/perf-ratchet.sh`: `perf stat -e instructions:u`,
    pinned, min-of-N. `LC_ALL=C` because `-x,` emits the locale's decimal separator, and a prefix
    match because perf suffixes the event name (`instructions:u`).

    A workload that did not exit 0 counts as unmeasured, not as a very small number. perf reports a
    count either way, so a missing binary otherwise reads as the process's own ~0.2M instructions —
    which the caller would print as a 50,000x speedup.
    """
    env = dict(os.environ, LC_ALL="C")
    best = None
    for _ in range(reps):
        try:
            p = subprocess.run(["perf", "stat", "-x,", "-e", "instructions:u"] + PIN + argv,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=300,
                               env=env)
        except Exception:
            return None
        if p.returncode != 0:
            return None
        for line in p.stderr.decode(errors="replace").splitlines():
            f = line.split(",")
            if len(f) >= 3 and f[2].startswith("instructions"):
                try:
                    v = float(f[0])
                except ValueError:
                    continue
                if best is None or v < best:
                    best = v
                break
    return best


def tier1_findings():
    """Equal JIT and interpreter instruction counts mean the JIT never ran on that benchmark.

    This is the check three benchmark reports in a row did not make. A row where Noeta-JIT and
    Noeta-int print the same number was read as "the JIT gives no speedup on maps" — a statement
    about native code. It was not: tier 1 declines any loop containing a non-native op, so those
    benchmarks never reached the JIT at all and both columns were the *same interpreter*. The
    difference matters because the two readings point at opposite work: one says optimize the
    native codegen, the other says make one more op native.

    Every row is corroborated against `noeta run --jit-stats`, which knows the answer directly, so
    a wrong verdict has to survive two instruments disagreeing in print. Returns the exit code:
    2 if any row could not be measured or the two instruments contradict each other — an
    unmeasured row is never a pass — and 0 otherwise. "The JIT is not running" is a finding about
    the engine, not an instrument failure, so it does not by itself set an exit code.
    """
    jit_bin, int_bin = os.path.join(SP, "noeta-jit"), os.path.join(SP, "noeta-int")
    print(f"\nTier-1 coverage — instructions retired, min of {ICOUNT_REPS}, pinned to CPU 3")
    print(f"  (equal counts = the JIT never ran; equal means within {TIER1_EQUAL_PCT}%)")
    print(f"  JIT: {jit_bin}\n  int: {int_bin}")
    status = 0
    # The `int` column has to be a JIT-less build or the comparison is JIT-vs-JIT, where a small gap
    # is just compile-thread timing and means nothing either way. Ask the binary once, up front —
    # a wrong bin dir is otherwise invisible until someone wonders why the speedups vanished.
    int_probe = jitstats.read(int_bin, os.path.join(XL, BENCHES[0] + ".noe"))
    if int_probe.status is not DeclineStatus.NO_JIT:
        print(f"  !!   the int column answers --jit-stats with a report, so that binary HAS a JIT "
              f"({int_probe.tier1_note()}).\n"
              f"       This is not a JIT-vs-interpreter comparison and every row below is suspect.")
        status = 2
    for bch in BENCHES:
        src = os.path.join(XL, bch + ".noe")
        j, i = icount([jit_bin, "run", src]), icount([int_bin, "run", src])
        if j is None or i is None or not i:
            miss = "JIT" if j is None else "int"
            print(f"  {bch:11s} ??   CANNOT MEASURE — perf counted no instructions for the {miss} "
                  f"binary. This is not a pass.")
            status = 2
            continue
        stats = jitstats.read(jit_bin, src)
        # Signed: positive is the JIT column doing fewer instructions than the interpreter, which is
        # the only shape a win can have.
        win = (i - j) / i * 100.0
        counts = f"{j / 1e6:.1f}M vs {i / 1e6:.1f}M"
        if abs(win) <= TIER1_EQUAL_PCT:
            print(f"  {bch:11s} !!   {counts} instructions, {abs(win):.2f}% apart — "
                  f"THE JIT IS NOT RUNNING HERE.")
        elif win < 0:
            print(f"  {bch:11s} !!   {counts} instructions — the JIT column does {-win:.1f}% MORE "
                  f"work than the interpreter. Not a win, and not native code.")
        else:
            print(f"  {bch:11s} ok   JIT is {i / j:.2f}x the interpreter "
                  f"({counts} instructions, {win:.1f}% fewer)")
        print(f"{'':16s}--jit-stats: {stats.tier1_note()}")
        if stats.status.loud:
            print(f"{'':16s}reproduce: {jit_bin} run --jit-stats {src}")
            status = 2
        # The two instruments must agree. Each disagreement below is a real misreading this check
        # exists to catch: a JIT-less binary in the JIT column, or a report claiming native code on
        # a row whose instruction counts say both binaries did identical work.
        elif win > TIER1_EQUAL_PCT and stats.status is DeclineStatus.NO_JIT:
            print(f"{'':16s}!! DISAGREEMENT: the counts differ but the JIT column's binary has no "
                  f"JIT — that gap is two builds, not native code.")
            status = 2
        elif (abs(win) <= TIER1_EQUAL_PCT and stats.status is DeclineStatus.NONE
              and (stats.osr_windows or stats.native)):
            print(f"{'':16s}!! DISAGREEMENT: --jit-stats reports {stats.native} native prototype(s) "
                  f"and {stats.osr_windows} OSR loop window(s) with nothing declined, yet the two "
                  f"binaries retired the same instructions. Check by hand.")
            status = 2
    return status


def main():
    os.chdir(XL)
    # The tier-1 check needs no wall-clock field at all now that it reads instructions retired, and
    # the field is the slow half — so it can be asked on its own, which is also how it gets tested.
    if "--tier1-only" in sys.argv[1:]:
        return tier1_findings()
    langs = [(n, b) for n, b in LANGS if have(b("empty"))]
    missing = [n for n, b in LANGS if not have(b("empty"))]
    print("Detected: " + ", ".join(n for n, _ in langs))
    if missing:
        print("MISSING (skipped): " + ", ".join(missing))
    print(f"reps={REPS} fields={FIELDS}\n")

    raw = {}          # (kind, lang) -> [per-field min ms]
    outputs = {}      # bench -> lang -> output
    for f in range(FIELDS):
        print(f"--- field run {f+1}/{FIELDS} ---", flush=True)
        for n, b in langs:
            ms, out = run_min(b("empty"))
            raw.setdefault(("startup", n), []).append(ms)
            if out and out.startswith("ERR:"):
                print(f"  !! {n} empty: {out}", flush=True)
        for bch in BENCHES:
            line = f"  {bch:10s}"
            for n, b in langs:
                ms, out = run_min(b(bch))
                raw.setdefault((bch, n), []).append(ms)
                outputs.setdefault(bch, {})[n] = out
                line += f" {n}={ms:.1f}" if ms is not None else f" {n}=FAIL"
            print(line, flush=True)
        print(flush=True)

    def best(key):
        vals = [v for v in raw.get(key, []) if v is not None]
        return min(vals) if vals else None

    names = [n for n, _ in langs]
    w = 11
    print("=" * 100)
    print("Startup (ms, min over all field runs):")
    print("".join(n.rjust(w) for n in names))
    print("".join((f"{best(('startup', n)):.1f}" if best(("startup", n)) else "-").rjust(w)
                  for n in names))
    print()
    print(f"Compute (ms, min-total over {FIELDS} field runs of min-of-{REPS}, minus min startup):")
    print("bench".ljust(11) + "".join(n.rjust(w) for n in names))
    print("-" * (11 + w * len(names)))
    table = {}
    for bch in BENCHES:
        row = bch.ljust(11)
        table[bch] = {}
        for n in names:
            t, s = best((bch, n)), best(("startup", n))
            v = None if (t is None or s is None) else max(0.0, t - s)
            table[bch][n] = v
            row += (f"{v:.1f}" if v is not None else "FAIL").rjust(w)
        print(row)

    # Wall-clock deltas, so read them as a smell test and nothing more: a sibling build running
    # during one field run and not the next moves this table by more than any regression it could
    # report. `measure.sh <binary>` on each of the two binaries is what settles a regression.
    print("\nRegression vs the 2026-08-02 binaries (7e7d038db) (same session, same machine):")
    print("  wall-clock — indicative only; confirm any delta with measure.sh (instructions retired)")
    print(f"{'bench':11s}{'JIT now':>10s}{'JIT@0802':>10s}{'delta':>10s}"
          f"{'int now':>10s}{'int@0802':>10s}{'delta':>10s}")
    for bch in BENCHES:
        r = table[bch]
        def d(a, b):
            if r.get(a) is None or r.get(b) is None or not r[b]:
                return "-"
            p = (r[a] - r[b]) / r[b] * 100.0
            return f"{p:+.0f}%"
        def f1(k):
            return f"{r[k]:.1f}" if r.get(k) is not None else "-"
        print(f"{bch:11s}{f1('Noeta-JIT'):>10s}{f1('JIT@0802'):>10s}"
              f"{d('Noeta-JIT', 'JIT@0802'):>10s}"
              f"{f1('Noeta-int'):>10s}{f1('int@0802'):>10s}"
              f"{d('Noeta-int', 'int@0802'):>10s}")

    status = tier1_findings()

    print()
    for bch in BENCHES:
        vals = {v for v in outputs.get(bch, {}).values() if v is not None}
        if len(vals) > 1:
            print(f"!! OUTPUT MISMATCH {bch}: {outputs[bch]}")
        fails = [n for n, v in outputs.get(bch, {}).items() if v is None or str(v).startswith("ERR")]
        if fails:
            print(f"!! FAILED {bch}: {fails} -> "
                  f"{ {n: outputs[bch][n] for n in fails} }")
    with open("raw-latest.json", "w") as fh:
        json.dump({f"{k[0]}|{k[1]}": v for k, v in raw.items()}, fh, indent=1)
    print("raw per-field timings -> raw-latest.json")
    return status

if __name__ == "__main__":
    sys.exit(main())
