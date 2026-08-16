#!/usr/bin/env python3
"""Cross-language micro-benchmark, 3 pinned field runs, with an in-session A/B against the
2026-07-17 baseline binaries.

Same method as xrun.py (taskset-pinned, min-of-N after warmup, startup-subtracted) but:
  * raw totals and raw startups are kept per field run, and compute is reconstructed as
    min(total over all field runs) - min(startup over all field runs) — the documented
    method, which per-run subtraction only approximates;
  * the cached 07-17 binaries run in the SAME session, so a regression is measured against
    the old code on today's machine rather than against a two-week-old table.
"""
import subprocess, time, shutil, os, sys, json

REPS = int(os.environ.get("BENCH_REPS", "9"))
FIELDS = int(os.environ.get("BENCH_FIELDS", "3"))
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

# How close the JIT and interpreter columns have to be before we stop calling it "the JIT did not
# help much" and start calling it "the JIT did not run". 3% is comfortably inside the run-to-run
# noise of a wall-clock field run on this box, so anything under it carries no information about
# native code at all.
TIER1_EQUAL_PCT = 3.0


def tier1_findings(table):
    """Equal JIT and interpreter columns mean the JIT never ran on that benchmark.

    This is the check three benchmark reports in a row did not make. A row where Noeta-JIT and
    Noeta-int print the same number was read as "the JIT gives no speedup on maps" — a statement
    about native code. It was not: tier 1 declines any loop containing a non-native op, so those
    benchmarks never reached the JIT at all and both columns were the *same interpreter*. The
    difference matters because the two readings point at opposite work: one says optimize the
    native codegen, the other says make one more op native.

    `noeta run --jit-stats` names the ops that blocked compilation, so when the columns match this
    asks the binary directly and prints the answer instead of leaving it to be inferred.
    """
    jit_bin = os.path.join(SP, "noeta-jit")
    print("\nTier-1 coverage (equal JIT/interp columns = the JIT never ran on that benchmark):")
    for bch in BENCHES:
        j, i = table.get(bch, {}).get("Noeta-JIT"), table.get(bch, {}).get("Noeta-int")
        if j is None or i is None or not i:
            print(f"  {bch:11s} -- (one of the two columns did not measure)")
            continue
        gap = abs(j - i) / i * 100.0
        if gap > TIER1_EQUAL_PCT:
            print(f"  {bch:11s} ok   JIT is {i / j:.2f}x the interpreter ({gap:.0f}% apart)")
            continue
        blocked = declined_ops(jit_bin, bch)
        print(f"  {bch:11s} !!   columns are {gap:.1f}% apart — THE JIT IS NOT RUNNING HERE.")
        if blocked:
            print(f"{'':14s}tier 1 declined; blocked by: {', '.join(blocked)}")
            print(f"{'':14s}reproduce: {jit_bin} run --jit-stats {bch}.noe")
        else:
            print(f"{'':14s}...and --jit-stats reports no declined loop either, so the two")
            print(f"{'':14s}   binaries may simply be equally fast here. Check by hand.")


def declined_ops(binary, bench):
    """The ops that kept tier 1 from compiling this benchmark, straight from --jit-stats."""
    if not os.path.isfile(binary):
        return []
    try:
        p = subprocess.run([binary, "run", "--jit-stats", bench + ".noe"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=180)
    except Exception:
        return []
    err = p.stderr.decode(errors="replace")
    if "declined tier 1" not in err:
        return []
    tail = err.split("declined tier 1", 1)[1]
    ops = []
    for line in tail.splitlines()[1:]:
        parts = line.split()
        # `  <file>:<line>  <Op>  <disassembly…>` — the op is the second field. A `— blocked by:`
        # header line names the prototype, not an op.
        if len(parts) >= 2 and "blocked by" not in line and ":" in parts[0]:
            if parts[1] not in ops:
                ops.append(parts[1])
    return ops


def main():
    os.chdir(XL)
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

    print("\nRegression vs the 2026-08-02 binaries (7e7d038db) (same session, same machine):")
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

    tier1_findings(table)

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

if __name__ == "__main__":
    main()
