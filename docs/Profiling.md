# Profiling (`noeta profile`)

`noeta profile app.noe` runs a program and reports **where it spends its time** — a hot-function table or a flamegraph. Like the debugger, it is a dev-time tool over the **production VM**: the same `load → check → compile → run` pipeline `noeta run` uses. By default a profile runs **tier-0** — tier-0 is the interpreter, tier-1 the JIT — with the JIT unarmed so every frame is observable. `--jit` arms tier-1 instead and samples native code at the trampoline, so the profile reflects what actually ships. [*Tiers, and what they mean*](#tiers-and-what-they-mean) covers the trade-off.

## Three profilers

| Mode | Flag | What it measures | Exact? |
|---|---|---|---|
| **Sampling** (default) | *(none)*, `--hz`, `--every` | A periodic snapshot of the call stack → a **flamegraph** of where wall-time goes. | No — statistical |
| **Instrumenting** | `--instrument` | Every call observed: exact per-function **call counts** and **self / total time** — and the exact call tree, so it renders a flamegraph too (nanosecond-weighted). | Yes — exact |
| **Allocation** | `--alloc` | Every **byte allocated**, attributed to the call path that allocated it — a memory flamegraph. Answers *"who allocates"* (churn/pressure); frees are ignored, so it is not a retention snapshot. | Yes — exact |

Reach for **sampling** to see the shape of a run (which paths dominate) at low overhead; reach for **instrumenting** when you need exact counts or precise self-time and can afford to observe every call; reach for **allocation** when the cost you are hunting is memory churn — a wall-time flamegraph actively hides it (allocation's price is paid later, in the allocator and the collector).

A flamegraph is stacks × a *weight*: wall samples, executed ops (`--every`), exact nanoseconds (`--instrument`), or allocated bytes (`--alloc`) — same picture, four different questions.

## Threads

Async tasks (`spawn` / `concurrent`) are cooperative and always profile into the main flamegraph. **Worker isolates** (`isolate f(args)`) run on their own OS threads and get their **own profile each**, named `isolate <fn> #<n>` — every mode produces one per isolate alongside `main`'s. A speedscope artifact carries them as separate profiles — the VS Code view shows a **thread picker** in the header (and speedscope.app its own profile selector) whenever a run spawned isolates; the folded/SVG forms root each isolate's stacks at its name.

The program's own stdout is always forwarded untouched — the profile report goes to **stderr** (or a file with `-o`), so a program you profile stays pipeable.

## Sampling — flamegraphs

```console
$ noeta profile app.noe
noeta profile: 48120 samples over 214 stacks, program ran in 631.204ms (tier-0, sampling, wall-clock 1000 Hz)
  render_frame     31820   66.1%
  collide           9004   18.7%
  integrate         4210    8.7%
  …
```

The default output is the **top functions by self-time** (the innermost frame of each sample). To get the full flamegraph, pick a format and (recommended) an output file:

```console
$ noeta profile app.noe --format svg -o app.svg          # open app.svg in a browser
$ noeta profile app.noe --format speedscope -o app.json  # open at speedscope.app
$ noeta profile app.noe --format folded -o - | inferno-flamegraph > app.svg
```

| Flag | Effect |
|---|---|
| `--hz <N>` | Wall-clock sampling rate (default **1000** Hz). |
| `--every <N>` | **Deterministic** sampling: one sample every `N` executed ops instead of on a wall clock. Reproducible run to run — an op-weighted (not time-weighted) flamegraph. Use it for stable diffs or scripted checks. |
| `--lines` | Attribute each flamegraph leaf to its **source line** (`fn:line`), not just the function — so the hot *line* within a function is visible. |
| `--jit` | Arm the **tier-1 JIT** while sampling (default: tier-0). Hot prototypes run native and their wall time is sampled at the JIT trampoline, so the profile is the *shipped* time distribution, not the interpreter's. Tier-1 frames are labeled ` [jit]` (e.g. `hot [jit]`), so a function's native and interpreter samples read apart. Wall-clock sampling only (the op-clock can't see native code); function-level, not line-level, inside JIT frames. The summary reports how many prototypes were promoted. |
| `--format <fmt>` | `folded` (Brendan-Gregg collapsed stacks), `svg` (self-contained flamegraph), `speedscope` (JSON for [speedscope.app](https://www.speedscope.app); each frame carries structured `file`/`line`/`col`, so tools can jump to source). |
| `-o <file>` | Write the artifact to a file instead of stderr (recommended for `svg`/`speedscope`). `-o -` writes it to **stdout** for piping — it follows the program's own forwarded output, so it suits programs that print little or nothing. |

### Determinism

Wall-clock sampling is statistical: two runs differ. `--every <N>` samples on an **op clock** instead, so the folded output is byte-identical across runs — reproducible flamegraphs for tests or before/after comparison. It weights by work (ops), not wall-time.

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
- **total** — inclusive time (the function and everything it called), counted at the outermost activation so recursion is not double-counted.

The instrumenting run also records the **exact call tree**, so it renders a flamegraph too — weighted by measured self-nanoseconds rather than sample counts, every call accounted, tiny programs included. `--format json` emits one artifact carrying both the table rows *and* the speedscope-shaped stacks (what the VS Code view renders as Flame Graph | Functions); the stack formats work directly as well: `--format svg` / `folded` / `speedscope`, counters labeled `ns`.

## Allocation — the memory flamegraph

```console
$ noeta profile app.noe --alloc
noeta profile: 6721576 bytes allocated over 4 stacks, program ran in 342.403ms (tier-0, alloc)
```

Every byte the program allocates is attributed — **exactly, not sampled** — to the call path that allocated it: the binary's counting allocator maintains a per-thread cumulative byte counter, and the profiler banks each delta onto the executing stack. The result answers *"who allocates"* — the churn/pressure question a wall-time flamegraph actively hides, because allocation's price is paid later (in the allocator and the collector). Frees are deliberately ignored: this is not a retention snapshot.

The stack formats apply as in sampling (`--format svg` / `folded` / `speedscope` — weights in bytes); there is no function table (stacks are the whole story). Notes:

- Only the interpreter thread's allocations are attributed to `main`'s stacks; each **isolate** has its own counter and its own profile (see [Threads](#threads)).
- A [composed toolchain](Native-Extensions) binary that doesn't register the counting allocator reports zero bytes — the summary says so explicitly rather than showing an empty graph.

## In VS Code

The [Noeta extension](Editor-and-AI-Tooling) has the profiler built in. **Noeta: Profile File (Sampling)** (also in the editor's run-button dropdown) profiles the active `.noe` file and opens the result in a **flame graph view** — click to zoom, double-click (or ctrl/cmd+click) a frame to jump to its source, with a sortable per-function table on the second tab. The profiled program's own output streams to the *Noeta Profile* output channel, and the hot **source lines get annotated in place** with their share of samples (cleared on edit, or with *Noeta: Clear Profile Line Annotations*). **Noeta: Profile File (Instrumenting)** opens the same Flame Graph | Functions pair with the *exact* call tree and table, and **Noeta: Profile File (Allocations, memory flamegraph)** opens the bytes-weighted view. When the profiled program spawned isolates, a **thread picker** in the view's header switches between `main` and each `isolate <fn> #<n>` profile.

The view is a renderer for the standard artifacts above: it opens any `*.noeprof.json` file — speedscope JSON (whose frames carry structured `file`/`line`/`col`) or the instrumenting JSON — so a profile taken on the CLI drops into the editor view, and the same file still loads at [speedscope.app](https://www.speedscope.app).

## Tiers, and what they mean

By default a profile session runs **tier-0** (the interpreter, JIT unarmed) — the same decision the [debugger](Debugging) makes — because the sampler needs an observable instruction boundary and the instrumenting counter needs to see every call. That has one honest consequence: the profile reflects the **interpreter's** time distribution. This is faithful for the questions a language-level profiler answers — *which function / line is hot, and how many times is it called* — because tier-0 preserves the exact call structure and relative work. It is **not** the absolute wall-time of the JIT-compiled build (the JIT changes constants, not shape). Call counts are tier-independent and exact.

**`--jit` (tier-1 sampling)** arms the production hot-counter JIT so hot prototypes run native, and the sampler attributes their wall time — the profile then *is* the shipped time distribution. Native code hits no interpreter instruction boundary, so the sampler cannot poll inside it; instead it polls at the **JIT trampoline** — the seam the VM crosses to enter and leave a compiled frame — and banks the wall time a native segment took onto the function that ran, labeled ` [jit]`. That makes tier-1 attribution **function-level**: a `[jit]` frame names the hot function, but there is no per-line breakdown inside native code (several source lines fuse into one native segment, so a single leaf line would be dishonest — `--lines` has no effect on `[jit]` frames). A function typically appears twice — `hot` (its tier-0 warm-up and any deopt bail-outs) and `hot [jit]` (its native samples) — which is the honest picture of a tiered runtime. The summary reports the promotion count (how many prototypes went native); [`--jit-stats`](The-CLI) on `noeta run` gives the full compile/bail report. Tier-1 sampling is wall-clock only — the deterministic op-clock (`--every`) can't observe native code (native ops don't advance the op counter), so it stays tier-0 even with `--jit`.

## Under the hood (short version)

Every profiler rides **one seam**: a hook the VM consults before each instruction (the cheaper twin of the debugger's pause seam), free when no profiler is attached. The allocation collector adds one ingredient: the binary's global allocator counts bytes per thread, and the hook banks each per-op delta onto the executing stack — the allocator is the single choke point, so native builtins' allocations are counted too. The instrumenting collector diffs the live frame stack to detect call enter/exit and times each; the sampler snapshots the live stack at a safe point when a tick is pending — **cooperative sampling**, so the timer thread never races the interpreter's stack. Under `--jit` the sampler adds one more safe point: the **JIT trampoline** (`jit_enter`/`jit_exit`), where it records which compiled frame is about to run and, on the way out, banks the wall time that accrued while native code ran onto that frame — so native time lands on the function that spent it instead of being misattributed to whatever interpreter frame resumed after a bail. The hooks are default no-ops, so an off-JIT (or unprofiled) run pays nothing. Function names and source lines come from the **always-emitted line tables** on every compiled chunk, so no special debug build is needed. The profiler is a dev tool, outside the [differential oracle](Architecture-and-Pipeline) (its signal is time, not program output).

## Current limitations

- **Tier-1 attribution is function-level** — `--jit` samples native code at the trampoline and names the hot function (` [jit]`), but there is no per-line breakdown inside a JIT frame; native segments fuse several source lines, so a leaf line would be dishonest. `--lines` still applies to the tier-0 frames of the same run, so a `--jit --lines` profile mixes the two — `inner:3` for an interpreted frame beside `inner [jit]` for its compiled one.
- **Isolate profiles are separate, not merged** — each worker isolate gets its own named profile (see [Threads](#threads)); there is no combined cross-thread view or merged function table.
- **Per-line self-time** in the instrumenting table is function-granular today (line attribution is a sampling feature via `--lines`).
