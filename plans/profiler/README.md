# Dev profiler / flamegraph — `noeta profile`

**Status: P0–P4 COMPLETE — arc done, all gates met.** This is the reconnaissance-backed scope for the built-in dev
profiler, the sibling of `noeta dap` / `noeta lsp` in the dev-tooling cluster. Like DAP, it is a
dev-time introspection tool over the **production bytecode VM**, and — because its signal is
wall-time and call structure, not program output — it lives **outside the differential oracle**
(same exemption as DAP/LSP and, on the production side, Network/Telemetry). It was written after
mapping the VM's frame stack, its per-op consult seam, the JIT hotness counter, and the compiler's
line tables (findings summarized inline, with exact `file:line` as of branch `profiler` off `main`).

It is a **payoff on the DAP investment.** Everything the profiler needs to turn a running frame
into a labelled `function @ file:line` — the explicit `Frame` stack, the per-op `span`s, the
`Chunk.line_table` pc→line index, `Chunk.name`, and `SourceMap::line_col` — DAP already built or
proved out. The profiler adds two things DAP did not need: a **counting/timing seam** at call
boundaries (instrumenting), and a **cooperative-sampling seam** at op boundaries (sampling).

> **Sibling, not this arc: native OTEL.** "Observability" was one roadmap word for two features.
> This plan is the **dev-time** half (profiler/flamegraph, no Host capability, differential-exempt
> because it is a tool *about* a run, not a *runtime* effect). The **production** half — a
> `Telemetry` Host capability with OTLP export — is a separate, later arc and is out of scope here.

---

## What a profiler needs (the axes)

1. **Attribute execution to source** — turn a live execution position (`proto`, `pc`) into a
   `function name @ file:line`, and a running call chain into a labelled stack.
2. **Instrumenting measurement** — exact per-function **call counts** and **self / total time**,
   by observing every call enter/return.
3. **Sampling measurement** — a periodic snapshot of the live call stack, aggregated into a
   **wall-time flamegraph** (which frames the program actually spends time under).
4. **Emit** — a human summary (top functions by self-time) and machine artifacts (folded stacks,
   an SVG flamegraph, speedscope JSON) a developer can open or pipe onward.

---

## The backend: profile the production VM (`noeta-vm`), tier-0

**We profile what actually ships**, exactly as DAP does. `noeta profile foo.noe` runs the same
`load → check → compile → VM` pipeline as `noeta run`, on the register VM. Three properties of
that VM make the profiler cheap; all three were already established by the DAP recon.

### The frame stack is the call stack (free)

`struct Frame { proto, base, pc, ret_dst, ret_transform, upvalues }`
(`crates/noeta-vm/src/lib.rs:551`) is a real call frame. The active stack is a Rust local
`frames: Vec<Frame>` threaded through the dispatch loop (`fn run(&mut self, mut frames, mut regs)`,
`lib.rs:2880`) — pushed on every call (interpreter sites `lib.rs:3680,3811,4020,…`; native-ABI
sites `lib.rs:1290,1316`) and popped on return (`lib.rs:4775,5693,7012`). Reading `frames` top-down
**is** a stack trace; DAP's `DebugView`/`DebugFrame` (`lib.rs:161,197`) already package exactly this.

### Names + lines are **always** emitted — no debug compile needed (the key finding)

DAP had to request a *debug* compile to preserve locals. **The profiler does not.** The
compiler's line-info tier is *unconditional* cold metadata (`crates/noeta-bytecode/src/lib.rs`):

- `Chunk.name: Option<String>` (`lib.rs:1043`) — every function's name, always present.
- `Chunk.def_span: Option<Span>` (`lib.rs:1046`) — its defining span, always present.
- `Chunk.line_table: Vec<LineEntry { pc, span }>` (`lib.rs:1064,1078`) — the DWARF-style,
  always-emitted, *"pure cold metadata the dispatch loop never touches"* index, resolved by
  `Chunk::line_span(pc)` (`lib.rs:1109`, a `partition_point` over `pc`). `SourceMap::line_col`
  (`crates/noeta-span/src/lib.rs:214`) turns the span into a 1-based line.

Only `Chunk.debug_locals` (`lib.rs:1054`, `LocalDebug`) is debug-gated — and the profiler does not
need locals. So `(proto, pc) → "function @ file:line"` works off an **ordinary compile**. This is
what the roadmap meant by *"reuses the two-tier line tables DAP built"*, and it is even cheaper than
expected: the profiler rides the *always-on* tier, so a profiled build is byte-identical to a normal
one except for the JIT decision below.

### The JIT — pinned off while profiling (a tier decision, like DAP)

The adaptive Cranelift JIT (gate `if self.jit.is_some() || … || self.aot` at `lib.rs:3019`) is
opaque to op-boundary observation: inside a native region there is no tier-0 pc to attribute to and
no op boundary to sample at. So a profile session **runs pure tier-0**, via the existing
JIT-off entry shape (`execute(module, host, jit=false)`, `lib.rs:1775`; `run_module`, `lib.rs:256`)
— the same decision, and the same plumbing, DAP uses. See *Honesty & the tier-1 deferral* below for
exactly what this does and does not distort, and why the honest frame contract makes tier-1 sampling
a clean later add rather than a rewrite.

### The instrumenting counter is a *new* counter, not the JIT's (a correction)

The roadmap says instrumenting is "nearly free on top of the JIT's existing per-prototype
frame-entry counter" (`jit_counters: Vec<u32>`, `lib.rs:804`, bumped in `jit_maybe_compile`,
`lib.rs:2185`; threshold `JIT_HOT_THRESHOLD = 50`, `lib.rs:890`). The *pattern* is right, but that
counter is **not** a reusable call-count source: it is `#[cfg(feature = "jit")]`, only bumped on the
JIT trampoline path, and **plateaus** — once a proto compiles, `jit_enter` takes the compiled path
and stops incrementing. So it measures *hotness up to the tier-up point*, not *total calls*. The
instrumenting profiler therefore adds its own `Vec<u64>` call counter bumped at the frame-push seam,
active only under a profile session. Trivially cheap (one increment per call, gated), just not the
same field. (This is the kind of precise correction the DAP plan made about stale line numbers —
recorded here so the "nearly free" claim is honest.)

### The seam already exists in the dispatch loop

DAP added a per-op consult, gated so it is free when unattached
(`if self.debugger.is_some()` at `lib.rs:3067`, handing a read-only `DebugView { module, frames,
regs }` to `Debugger::before_op`, `lib.rs:75`). The sampling profiler wants the *same shape* — a
per-op check that is a single predicted branch when disarmed — and the instrumenting profiler wants
a *cheaper* one at call boundaries only. Both ride `Option`-gated fields on `Vm`, `None` on a normal
`noeta run`, so **the non-profiled hot path is unchanged** (guarded by the standing VM bench gate).

---

## Why cooperative sampling (not signal/thread stack-walking)

A classic sampling profiler fires a timer signal (`SIGPROF`) and walks the stack in the handler, or
has a second thread walk the target thread's stack. **Neither is safe here:** `frames` is a Rust
`Vec` local owned by the VM worker thread — a second thread reading it races the owner (and the
`Vec` can realloc mid-walk), and an async signal handler cannot safely touch the interpreter's Rust
state. So the profiler uses **cooperative sampling**, the standard managed-runtime answer: a
background **timer thread** ticks at a fixed rate and sets a shared `AtomicU32` "samples pending"
counter; the VM **polls that atomic at op boundaries** (the DAP seam's cheaper twin) and, when a
tick is pending, walks *its own* live `frames` at that safe point. No data race, no signal handler,
no `unsafe`. The only cost when armed is one relaxed atomic load per op — the DAP branch's twin —
and a stack copy per sample (~1 kHz, negligible).

### Determinism: a wall-clock mode and a reproducible op-clock mode

Wall-time sampling is inherently nondeterministic — good for real profiles, useless for a test
oracle. The project's determinism discipline (logical clock, seeded RNG) has a natural analogue
here: **op-clock sampling** — instead of a wall-timer, sample every `N` executed ops. That yields a
byte-reproducible, *work-weighted* flamegraph (samples fall where ops are spent, not where
wall-time lands), which is what makes the sampling slices **testable with exact fixtures** rather
than flaky thresholds. Both modes share all downstream aggregation/rendering; the only difference is
what sets the pending-sample flag (a timer thread vs. an op counter in the poll). `--every <N>ops`
selects the deterministic mode; the default is wall-clock at `--hz <rate>` (default 1000).

---

## Honesty & the tier-1 deferral

Pinning tier-0 means a profile reflects the **interpreter's** time distribution, which is faithful
for the questions a language-level profiler answers — *which function / line does my program spend
its work under, and how many times is it called* — because tier-0 preserves the exact call
structure and relative work. It is **not** the absolute wall-time of the shipped tier-1 build (the
JIT changes constants, not shape). This is the identical tradeoff DAP accepted, and it is called out
so the artifact is not mis-sold as production wall-time.

The roadmap's *"stack-walking works interpreted or Tier-1"* is correct and stays reachable: the
JIT's honest per-frame contract (guards bail to tier-0 before mutating state; frames are pushed and
popped honestly even under tier-1) means the **stack walk itself is valid at any tier**. What tier-0
buys for *free* is the **sample trigger** (op boundaries exist to poll at). Tier-1 sampling therefore
does not need a rewrite — it needs a *trigger* inside native code, and the cheap path already exists:
poll the pending-sample atomic at the JIT trampoline points that are still Rust (`jit_enter`
`lib.rs:2131`, `jit_osr_backedge` `lib.rs:2290`), i.e. at calls and loop back-edges — exactly where
hot time concentrates — then walk the (honest) frames. That is the deferred richer add (see
*Deferred*), and this plan is structured so nothing in the tier-0 milestone blocks it.

---

## Decisions proposed (confirm before P0)

1. **Command name = `noeta profile <FILE>`** (decided with the user 2026-07-07). New `noeta-prof`
   crate + `Command::Profile` variant + `cmd_profile()`, mirroring `cmd_dap()` at
   `crates/noeta-cli/src/main.rs:233`. `--profile` is **taken** as the `noeta.toml` dev-tier
   selector on `run`/`test`/`bench`/`doc`/`build`/`dump` (`main.rs:55`, resolved via `manifest.rs`),
   but that is a **flag**, and `noeta profile` is a **subcommand** — different namespaces that never
   co-occur, because the profiler subcommand deliberately takes **no** `--profile`/`--tier` flag: it
   profiles the program **as written** (empty active-tier set, like `run` with no profile). So there
   is no real collision; the profiler owns the `profile` verb and the dev-tier concept keeps the
   `--profile` flag on the other commands. *(Considered and rejected: renaming the tier flag
   `--profile → --target` to free the word — but `--target` is the universal name for a
   *compilation target* (arch/OS/triple, WASM), which M3's WASM/native-AOT work will want, so that
   rename trades one collision for a worse future one. The subcommand/flag split avoids the rename
   entirely.) The internal crate stays `noeta-prof` for brevity (users only ever type `noeta
   profile`).
2. **Two modes under one command.** `noeta profile FILE` **samples** (wall-time flamegraph) by
   default; `noeta profile FILE --instrument` (or `--count`) runs the exact call-count/self-time
   collector. Rationale: sampling is the headline artifact; instrument is the exact, lower-overhead
   companion. *(Both pin tier-0; both reuse the same name/line resolution and emit layer.)*
3. **Cooperative sampling, not signals/cross-thread walking** (argued above). Timer thread sets an
   atomic; the VM polls it at the op-boundary seam and walks its own frames. Adds one `Option`-gated
   field + one gated poll to dispatch; zero cost disarmed.
4. **Tier-0 pinned during a profile session**, via the existing `jit=false` entry — same decision as
   DAP. Tier-1 sampling (trampoline-point trigger) is deferred, not designed out.
5. **New `noeta-prof` crate**, deps mirroring `noeta-dap` (`loader`, `check`, `compiler`,
   `bytecode`, `vm`, `stdlib`, `runtime`, `serde_json`) plus **`inferno`** (pure-Rust folded→SVG
   flamegraph renderer; no perl/`flamegraph.pl`, no external process). Keeps profiler weight out of
   the CLI except at the `cmd_profile()` entry, exactly like `noeta-dap`/`noeta-lsp`.
6. **Emit formats:** `--format folded` (Brendan-Gregg collapsed stacks, the universal interchange),
   `--format svg` (default for sampling; via `inferno`), `--format speedscope` (JSON, opens in
   speedscope.app), and for `--instrument` a sorted **text table** (default) + `--format json`.
   `-o <file>` writes the artifact; otherwise folded/table go to stdout and svg/speedscope require
   `-o`. Line-granular attribution (leaf line via `line_span`); column negotiation deferred.

---

## Architecture — where the pieces live

```
$ noeta profile app.noe [--instrument] [--hz N | --every Nops] [--format …] [-o out]
        │
        ▼
noeta-cli  Command::Profile → cmd_profile()
        │
        ▼
noeta-prof crate
  ┌───────────────────────────────────────────────────────────────┐
  │ session:  load → check → compile (ordinary) → run tier-0       │
  │ collector:  ┌ instrument: call-count Vec<u64> + enter/exit ns  │
  │             └ sample:     timer/op-clock → AtomicU32 pending    │
  │ resolve:  (proto,pc) → Chunk.name + line_span + line_col       │
  │ emit:     folded · inferno SVG · speedscope JSON · table       │
  └───────────────────────────────────────────────────────────────┘
        │  (two new Option-gated seams on Vm, None on a normal run)
        ▼
noeta-vm (tier-0)
  • frame-push/pop  → instrumenting counter+timer     (call-boundary seam)
  • op-boundary poll of AtomicU32 → walk `frames`      (sampling seam, DAP-branch twin)
  • names/lines from always-on Chunk line tables · SourceMap::line_col
```

New VM state: an `Option<ProfileHook>`-style field consulted at frame push/pop (instrument) and an
`Option<Arc<AtomicU32>>` + poll at the op-boundary (sample). Both gated; both absent on `noeta run`.

---

## What already exists vs. what we build

**Reused as-is (free / near-free):** the tier-0 VM + explicit `Frame` stack (`lib.rs:551,2880`);
`DebugView`/`DebugFrame` stack packaging (`lib.rs:161,197`); the always-on `Chunk.name` /
`def_span` / `line_table` and `Chunk::line_span` (`noeta-bytecode/src/lib.rs:1043–1109`);
`SourceMap::line_col` (`noeta-span/src/lib.rs:214`); the JIT-off run entry (`execute(…, jit=false)`
`lib.rs:1775`; `run_module` `lib.rs:256`); the `noeta-dap` crate shape (session compile+run, worker
thread) as the closest sibling template; the CLI subcommand pattern (`cmd_dap` `main.rs:233`,
dispatch `main.rs:220`).

**New plumbing (smallest→largest):**

- **`(proto,pc) → label` resolver** in `noeta-prof` (join `Chunk.name` + `line_span` + `line_col`).
  *Small — pure reuse.*
- **Instrumenting collector + call-boundary seam** — a `Vec<u64>` call counter + enter/exit
  timestamps at the frame push/pop sites, `Option`-gated on `Vm`; self/total accumulation on return.
  *Small–medium — one gated field, push/pop hooks.*
- **Sampling collector + op-boundary seam** — timer thread (or op-clock) → `AtomicU32` pending; a
  gated poll in `dispatch` that walks `frames` into an interned stack aggregator. *Medium — the
  load-bearing VM change, but it is the DAP consult branch's cheaper twin.*
- **Emit layer** — folded stacks, `inferno` SVG, speedscope JSON, instrument text/JSON table.
  *Medium, mechanical.*
- **`noeta profile` subcommand** — `Command::Profile` + `cmd_profile()` + flag parsing. *Small.*

---

## Slices

One demonstrable capability per slice. Like DAP/LSP this is I/O-facing and **outside the
differential**; each slice is tested with **in-process fixtures** — run a program under the
collector and assert the aggregated table / folded output — made *exact* by the deterministic
op-clock mode for sampling. The standing **VM bench gate** (M2.0 criterion benches) guards that a
non-profiled `run` shows no dispatch regression from the new gated seams.

| # | Slice | Delivers | Notes |
|---|-------|----------|-------|
| **P0** ✅ | Crate + `noeta profile` + tier-0 profiled run | `noeta profile app.noe` runs a program to completion under the profiler harness and prints the run's wall-clock time | **DONE.** New `noeta-prof` crate (deps mirror `noeta-dap`) + `Command::Profile`/`cmd_profile()`; `session.rs` = ordinary compile (no debug info — line tables are always-on) run via `run_module_debug(.., None)` (the tier-0 path, JIT never armed); program stdout forwarded verbatim, profile report to stderr (stays pipeable). Public `noeta_prof::{profile → Report, run}` split so the collector-free run is fixture-tested (4 tests: clean run, compile error, missing file, abort exit). Naming + tier-0 + crate-shape decisions landed. Op-count dropped from P0 (it needs the collector seam — P1/P2). |
| **P1** ✅ | Instrumenting profiler (counts + self/total) | Per-function table: calls, self, total, self% | **DONE.** One `Option`-gated per-op seam on `Vm` (`ProfileHook` trait + `profiler` field, consulted at the debugger's seam minus the pause) shared by both collectors — chosen over hooking the ~13 scattered frame push/pop sites. `InstrumentCollector` keeps a **shadow stack** reconciled against the live frames each op (fast-path = same innermost frame → one compare); enter = start timer + bump count, exit = bank `self = elapsed − children`, inclusive counted at the outermost activation (recursion-safe). Labels via always-on `Chunk.name` + `def_span`→line. `noeta profile --instrument`, self-time-sorted text table. **5 exact fixtures**: `fib(10)` counted at exactly 177 (2·Fib(11)−1), hot-leaf sorts first, leaf self==total, `outer` total ⊇ `inner`. Conformance/differential/leak all green (dispatch change behavior-neutral). `--format json` deferred to P3 (with the other emit formats). **Pinned dispatch A/B pending a quiet box** (measured under load ~20 → noise-dominated, 2× run-to-run swing; the added branch is one predicted-not-taken op, identical in shape to the shipped debugger consult beside it — pre-merge gate, not a per-slice blocker). |
| **P2** ✅ | Sampling profiler (wall-time flamegraph) | Folded stacks the program spends time under | **DONE.** `SampleCollector` on the same `ProfileHook` seam: a wall-clock **timer thread** (`spawn_timer`) bumps a shared `AtomicU32`, and the op-boundary poll reads+clears it and snapshots the live `frames` (root→leaf proto chain) into a `HashMap<Vec<u32>, count>` aggregator, weighted by ticks accrued (slow ops still time-weight). **Cooperative sampling** — the VM snapshots its own stack at a safe point; the timer thread never touches `frames`. Sampling is now the **default** `noeta profile` mode; `--hz N` sets the rate. **Deterministic op-clock `--every N`** (sample every N ops, no timer) lands here too → byte-reproducible folded output. Folded stacks (Brendan-Gregg `main;fib;fib N`) sorted heaviest-first (stable), emitted to stderr (program stdout stays clean). **6 fixtures**: op-clock determinism (identical across runs), hot leaf ≥80% of samples + rooted at `main`, counts sum to total, rate-proportionality, folded well-formedness, wall-clock smoke. No VM change (differential unaffected). `-o <file>` + SVG/speedscope rendering → P3. |
| **P3** ✅ | Rendering formats | An SVG flamegraph / speedscope profile you can open | **DONE.** `render` module + `Format {folded,svg,speedscope,table,json}`; `--format <fmt>` + `-o <file>` (artifact→file, else stderr — program keeps stdout). `inferno` (0.12, `default-features=false`) folded→**SVG**; **speedscope** JSON (`type: sampled`, shared frame table + weighted samples, opens at speedscope.app); instrument **`--format json`**. Format/mode mismatch (e.g. `svg` on `--instrument`) and unknown format both exit 2 before running. **6 fixtures**. zero unsafe (speedscope interns with owned keys). |
| **P3.1** ✅ | Line attribution + top-N | Hot *line* in the flamegraph; a hot-function summary | **DONE** (`82e1a9c`). `--lines` (sampling): `DebugView::pc_at` → sampler captures the leaf pc into the key only when on (0 otherwise → default output unchanged) → resolved to `fn:line` via the line table; `resolve_flamegraph` re-aggregates by resolved label so several pcs on one line merge. `top_functions()` aggregates samples by leaf fn → the sampling **default** output is now that top-N summary (not a folded dump; a machine artifact needs explicit `--format`). +3 fixtures. |
| **P4** ✅ | Docs + roadmap + memory | The arc is recorded | **DONE.** New `docs/Profiling.md` wiki page (two modes, formats, tier-0 honesty, determinism, cooperative sampling); `docs/The-CLI.md` command-table row + `docs/_Sidebar.md` entry; `docs/Performance-Techniques.md` cross-link; roadmap ticked (dev-profiler half of observability done); memory ([[profiler-arc]]). |

**Dispatch bench — gate met.** Pinned A/B on a quiet box (`taskset -c 3 cargo bench -p noeta-vm --
vm/dispatch_fib`, tight <0.2% spreads): baseline without the profiler consult **7.855 ms**, with it
**7.759 ms** (−1.2%). The per-op seam shows **no measurable regression** on a non-profiled run (the
consulted side is if anything marginally faster — code-layout jitter; a predicted-not-taken branch
can't genuinely speed a loop up). This matches the shipped debugger consult it sits beside.

P1 (instrument) before P2 (sample) matches the roadmap's "near-free first, richer add second," and
gives an exact, timer-free base before the sampling seam builds on the same crate.

---

## Deferred (revisit after the arc)

- **Tier-1 (JIT-on) sampling** — poll the pending-sample atomic at the still-Rust JIT trampoline
  points (`jit_enter` `lib.rs:2131`, `jit_osr_backedge` `lib.rs:2290`) so hot native regions get
  sampled at their calls/back-edges, then walk the honest frames. Gives production-tier wall-time.
  The tier-0 milestone is structured not to block this.
- **Allocation / memory profiling** — attribute heap allocations (and RC traffic) to call sites,
  reusing `noeta-alloc-probe`. A different collector on the same crate/emit spine.
- **Cross-isolate / multi-thread profiles** — merge per-isolate sample streams (each isolate has its
  own VM thread); start single-isolate / main-thread, like DAP.
- **Continuous / attach-to-running-`serve` profiling** — profile a live server over a control
  channel rather than a one-shot `prof FILE`. (Soft-follows the server work.)
- **Differential flamegraph / A-B compare** — diff two folded profiles (before/after a change),
  the profiler-side analogue of the H-BENCH interleaved compare.
- **Column-precise / sub-statement attribution** — start line-granular.
- **In-editor flamegraph view** — a VS Code panel; the folded/speedscope artifacts already open in
  external tools, so this is pure polish.

## Non-goals

Production telemetry (the OTEL Host-capability arc — separate). Profiling JIT'd regions in the
first milestone (tier-0 pinned, as DAP). A sampling *signal* handler / cross-thread stack-walk
(unsafe here; cooperative sampling is the design). Any change to the non-profiled hot path beyond
`None`-gated fields.

## Gate — this milestone is done when

`noeta profile app.noe` produces a wall-time flamegraph (folded + SVG) and `noeta profile app.noe
--instrument` produces an exact per-function call-count/self-time table, both over the tier-0
production VM with function+line labels from the always-on line tables; the deterministic
`--every Nops` mode makes the sampling fixtures exact; in-process fixtures cover both collectors and
the resolver; the VM benches show **no dispatch regression** on a non-profiled run; and the
workspace is clean under fmt/clippy with **zero new `unsafe`**.
