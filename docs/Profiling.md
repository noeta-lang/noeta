# Profiling (`noeta profile`)

`noeta profile app.noe` runs a program and reports **where it spends its time** — a hot-function
table or a flamegraph. Like the debugger, it is a dev-time tool over the **production VM**: the same
`load → check → compile → run` pipeline `noeta run` uses, with the JIT unarmed so every frame is
observable. What you profile is what ships (see [*Tier-0, and what it means*](#tier-0-and-what-it-means)).

> **Not `--profile`.** The `--profile <name>` *flag* on `run`/`test`/`build`/… selects a
> [`noeta.toml` build profile](Documentation-and-Tiers) (which dev tiers are live). The `noeta
> profile` *subcommand* is unrelated — it profiles a program's execution and takes no `--profile`
> flag.

## Two profilers

| Mode | Flag | What it measures | Exact? |
|---|---|---|---|
| **Sampling** (default) | *(none)*, `--hz`, `--every` | A periodic snapshot of the call stack → a **flamegraph** of where wall-time goes. | No — statistical |
| **Instrumenting** | `--instrument` | Every call observed: exact per-function **call counts** and **self / total time**. | Yes — exact |

Reach for **sampling** to see the shape of a run (which paths dominate) at low overhead; reach for
**instrumenting** when you need exact counts or precise self-time and can afford to observe every
call.

The program's own stdout is always forwarded untouched — the profile report goes to **stderr** (or a
file with `-o`), so a program you profile stays pipeable.

## Sampling — flamegraphs

```console
$ noeta profile app.noe
noeta profile: 48120 samples over 214 stacks, program ran in 631.204ms (tier-0, sampling, wall-clock 1000 Hz)
  render_frame     31820   66.1%
  collide           9004   18.7%
  integrate         4210    8.7%
  …
```

The default output is the **top functions by self-time** (the innermost frame of each sample). To
get the full flamegraph, pick a format and (recommended) an output file:

```console
$ noeta profile app.noe --format svg -o app.svg          # open app.svg in a browser
$ noeta profile app.noe --format speedscope -o app.json  # open at speedscope.app
$ noeta profile app.noe --format folded | inferno-flamegraph > app.svg   # via -o /dev/stdout
```

| Flag | Effect |
|---|---|
| `--hz <N>` | Wall-clock sampling rate (default **1000** Hz). |
| `--every <N>` | **Deterministic** sampling: one sample every `N` executed ops instead of on a wall clock. Reproducible run to run — an op-weighted (not time-weighted) flamegraph. Use it for stable diffs or scripted checks. |
| `--lines` | Attribute each flamegraph leaf to its **source line** (`fn:line`), not just the function — so the hot *line* within a function is visible. |
| `--format <fmt>` | `folded` (Brendan-Gregg collapsed stacks), `svg` (self-contained flamegraph), `speedscope` (JSON for [speedscope.app](https://www.speedscope.app)). |
| `-o <file>` | Write the artifact to a file instead of stderr (recommended for `svg`/`speedscope`). |

### Determinism

Wall-clock sampling is statistical: two runs differ. `--every <N>` samples on an **op clock**
instead, so the folded output is byte-identical across runs — reproducible flamegraphs for tests or
before/after comparison. It weights by work (ops), not wall-time.

## Instrumenting — exact counts and self-time

```console
$ noeta profile app.noe --instrument
noeta profile: 3 functions, program ran in 1.480s (tier-0, instrumenting)
function       calls          self         total   self%
work               1        1.180s        1.180s   79.8%  (app.noe:1)
fib           242785     298.843ms     298.843ms   20.2%  (app.noe:7)
main               1     480.173µs        1.480s    0.0%  (app.noe:1)
```

- **calls** — every activation, including recursive.
- **self** — time in the function's own body, excluding callees (the primary sort key).
- **total** — inclusive time (the function and everything it called), counted at the outermost
  activation so recursion is not double-counted.

`--format json` emits the same rows as JSON (`-o rows.json` to a file).

## Tier-0, and what it means

A profile session runs **tier-0** (the interpreter, JIT unarmed) — the same decision the
[debugger](Debugging) makes — because the sampler needs an observable instruction boundary and the
instrumenting counter needs to see every call. That has one honest consequence: a profile reflects
the **interpreter's** time distribution. This is faithful for the questions a language-level profiler
answers — *which function / line is hot, and how many times is it called* — because tier-0 preserves
the exact call structure and relative work. It is **not** the absolute wall-time of the JIT-compiled
build (the JIT changes constants, not shape). Call counts are tier-independent and exact.

## Under the hood (short version)

Both profilers ride **one seam**: a hook the VM consults before each instruction (the cheaper twin
of the debugger's pause seam), free when no profiler is attached. The instrumenting collector diffs
the live frame stack to detect call enter/exit and times each; the sampler snapshots the live stack
at a safe point when a tick is pending — **cooperative sampling**, so the timer thread never races
the interpreter's stack. Function names and source lines come from the **always-emitted line tables**
on every compiled chunk, so no special debug build is needed. The profiler is a dev tool, outside the
[differential oracle](Architecture-and-Pipeline) (its signal is time, not program output).

## Current limitations

- **Tier-0 only** — the JIT-compiled tier is not sampled (its absolute wall-time isn't reflected);
  see above. Sampling the JIT tier is a future add.
- **Single isolate / main thread** — cross-`isolate` (multi-OS-thread) profiles are not merged yet.
- **Per-line self-time** in the instrumenting table is function-granular today (line attribution is
  a sampling feature via `--lines`).
