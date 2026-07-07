# Debugger — `noeta dap` Debug Adapter Protocol server

**Status: D0–D5.2 COMPLETE — the milestone is done.** `noeta dap` runs over stdio, the VS Code/VSCodium
client launches on F5, and breakpoints, stepping, stack/scopes/variables, and `evaluate` — variable
paths, operators, **and calls** (`xs.len()`, `f(x)`, `p.mag()`) — all work; a hover stays
side-effect-free (paths/operators only, calls refused).

> **Superseded downstream (tooling-unification arc):** the D5/D5.2 evaluator described below — the
> read-only AST walker plus the by-name call dispatch — was later **replaced and deleted**. Every
> `evaluate` now COMPILES its fragment through a session adopted from the launch's checked compile
> (`plans/tooling-unification`): full language at the watch/console incl. **closures**
> (`xs.filter(fn(x) => x > 15)`) and statements, escapes surviving resume; hover runs the same
> engine gated to the read-only surface (an AST gate + a frame-push refusal at run time). This doc
> stays as the D-milestone record; the current architecture lives in the tooling-unification plan.

This is the next
milestone after the `noeta lsp` language server shipped and merged to `main`. It mirrors that arc: a new `noeta-dap` crate
plus a `noeta dap` CLI subcommand, speaking a stdio wire protocol to any DAP-capable editor (VS Code /
VSCodium's built-in debug UI, Neovim `nvim-dap`, etc.). Where the LSP was a thin *read* adapter over the
compiler's salsa graph, the DAP is a *control* adapter over the running program — so the load-bearing work
is different, and it is worth naming precisely before writing any code.

This document is the reconnaissance-backed scope. It was written after mapping the execution backends and
their introspection surface (findings summarized inline below).

---

## What a debugger needs (the five axes)

A DAP server must, at minimum:

1. **Set breakpoints** — map a source `(file, line)` to an execution position, and pause when reached.
2. **Pause / continue / step** — stop execution, resume it, and single-step (over / into / out).
3. **Report a call stack** — the chain of active function frames, each with a name and a source line.
4. **Inspect variables** — the named locals (and `self`/fields) live in each frame, rendered as values.
5. **Evaluate** (stretch) — run an expression in a paused frame's scope (watch / REPL / hover-eval).

---

## The backend: debug the production VM (`noeta-vm`)

**We debug what actually ships.** `noeta run foo.noe` compiles to a bytecode `Module` and executes on the
register-machine VM (`crates/noeta-cli/src/main.rs:270`,
`VmBackend::run_module_with_host_and_executor_parallel`). That is the backend the debugger drives. The
`noeta-eval` tree-walker is a *separate* engine used only as the differential conformance oracle (and to
power `noeta repl`); debugging it would mean debugging something other than production, so it is explicitly
**not** the target. (It is not even on the `run` path — `noeta-cli` links it only for the REPL.)

Two properties of the VM look hostile to debugging at first glance; both dissolve cleanly, which is why the
production backend is the right and achievable target rather than a heroic one.

### The JIT — turn it off in debug mode (it's a performance tier, not a semantics tier)

The adaptive Cranelift JIT (tier-1) is on by default and native-compiles hot prototypes; inside a JIT'd
region there is no observable pc and registers live in machine registers, so it is opaque to stepping. But
tier-1 exists purely for speed and is held byte-for-byte observably identical to the tier-0 interpreter by
the JIT's own correctness contract (guards bail to tier-0 before mutating state). So a debug session simply
**does not arm the JIT** and runs pure tier-0, which is fully introspectable.

This is small, already-supported plumbing, not a new engine:
- The tier gate is a **single branch** — `if self.jit.is_some()` at `crates/noeta-vm/src/lib.rs:2193`. With
  the JIT unarmed, execution never enters native code.
- A tier-0-only entry already exists and is exercised by the differential baseline: `VmBackend::run_module`
  (`lib.rs:85`) calls `execute(module, host, jit=false)` (`lib.rs:1277`). The debug driver uses this shape.
- All the debugger needs from production code here is to **not** call `init_jit()`. No JIT code is removed;
  the perf tier is simply dormant during a debug session.

### Variable names — the compiler keeps them in a debug-info side-table (the "something else")

The VM is a register machine, so at runtime a local is `regs[frame.base + i]` — a numeric slot with no name.
But the names are **not lost, they are discarded**: the compiler *has* every source name at lowering time and
throws it away after assigning a register. The fix is the standard one every real debugger uses (DWARF / PDB /
source maps): when compiling **for debug**, emit a side-table that preserves the mapping, off the hot path.

The exact emission sites already exist:
- **Locals:** `declare_local(name, src, owned, mutable)` at `crates/noeta-compiler/src/lib.rs:1922` binds a
  source `name` to a register `reg` (recording it in the compile-time scope map). In debug mode it also
  pushes `LocalDebug { name, reg, def_span, mutable }` to the chunk's debug table. Powers the Variables view.
- **Functions:** `declare_fn(name, func)` at `lib.rs:1378` knows each function's name and its proto index. In
  debug mode it records `proto → { name, def_span }`. Powers `stackTrace` frame names + source lines. (The
  compiler already reverse-maps methods/destructors via `Module.methods`/`destructors`; this closes the gap
  for free functions and gives every frame a name + defining span.)

The side-table stays deliberately minimal — `{ name, reg, def_span }` — because **types are not baked in; they
are reused** (see next section). It records only the *codegen* fact (slot assignment) that lives nowhere else.

**Register coalescing — pin named locals, don't skip the pass.** A bytecode→bytecode post-pass,
`regalloc::coalesce` (`lib.rs:1112`), merges registers with disjoint live ranges, after which one physical
register can host several source names — so a naive `reg → name` is not 1:1. The clean resolution reuses
machinery the pass **already has**: `coalesce` already *pins* every register in `chunk.frame_locals`
(`regalloc.rs:191`) to a unique, never-merged slot — today for destructor-teardown safety. The compiler
already accumulates every named local's register in `self.frame_locals` (`lib.rs:1942`); it merely *withholds*
that list from the chunk in the no-destructor case (`lib.rs:1081`) to keep benchmark/golden parity. So a debug
compile simply **emits `frame_locals` unconditionally**, and coalescing then pins each named local to its own
dedicated slot → a clean 1:1 `reg → name`, while temporaries (the bulk of register churn) still coalesce
freely. This is not a shortcut: it reuses trusted, shipped pinning logic (zero new register-allocation code),
keeps debug frames lean (only named locals stay unmerged), and leaves the compile close to production. The
only deltas from a production build — named locals unmerged, JIT off — are the intended `-O0`-style "debug
disables optimizations, semantics preserved" tradeoff, consistent with the JIT decision. The pc-scoped
"location list" answer (coalesce named locals too, track `(reg, pc-range) → name`) buys only *optimized-build*
debugging and is genuinely deferred.

### Types — reused from the checker and the runtime value, not re-derived

The debugger does **not** compute types. Once the side-table yields `(name, reg, def_span)` and the register
yields the runtime `Value`, a variable's type comes from two existing sources, both free:

- **The runtime value carries its own type.** Every `Value` has a reified `TypeRepr` tag (what makes
  `x is List<int>` precise at runtime), so the debugger renders a variable's *actual* type straight from the
  value — always available, correct even through `dyn`. This is the primary source for the Variables view.
- **The checker's *declared* type is one join away.** `def_span` is exactly the key into `noeta-check`'s
  `expr_types: HashMap<Span, TypeRepr>` (the IDE/`check_all_with_types` path the LSP already runs). So the
  static declared type is a map lookup — no new inference — for cases where it is preferable to the runtime
  tag (e.g. a slot currently holding `unit`, or showing the annotated type).

The LSP/checker are **reused wholesale for `evaluate` (D5)** too: a watch-window expression is validated by
`noeta-check` against the frame's environment before it runs. What is *not* reusable from the checker/LSP is
the `name → register` mapping itself — registers are a codegen artifact invented at lowering, invisible to a
span-keyed checker or resolver — which is precisely why the compiler-side side-table is the one irreducibly
new piece.

### Everything else on the VM is already debugger-shaped

- **Frames are explicit.** `struct Frame { proto, pc, base, .. }` (`lib.rs:252`) is exactly a call frame:
  which prototype, where in its bytecode, and its register-window start. A shadow stack is *not* needed — the
  VM already keeps one.
- **pc → source line is nearly free.** ~80 of 87 opcodes carry an inline `span` (`noeta-bytecode`), and
  `SourceMap::line_col` (`noeta-span`) resolves span→line. `module.protos[frame.proto].code[frame.pc].span`
  gives the current line directly. (The ~7 span-less housekeeping ops fall back to the nearest spanned op.)
- **line → pc** for setting breakpoints needs a small new reverse index (a one-pass scan per prototype);
  all the data is present.

**The one caveat that stays:** the frame stack lives as a **Rust local inside `dispatch`** (`lib.rs:2152`),
not on `&Vm`, so it is not reachable from outside the loop. The pause/step hook must therefore be **consulted
inside the dispatch loop**. That is the core interpreter change (see D1), and it is bounded: one hook consult
at the top of an instruction's handling, gated so it is free when no debugger is attached.

---

## Decisions proposed (confirm before D0)

1. **Backend = the production bytecode VM (`noeta-vm`), JIT unarmed in debug sessions.** As argued above.
2. **Variable/function names via a minimal compiler debug-info side-table** (`{ name, reg, def_span }`),
   emitted only for debug compiles. Clean 1:1 `reg → name` is achieved by **pinning named locals through
   coalescing** — emit `frame_locals` unconditionally in debug mode, reusing the existing destructor-teardown
   pin path (not by disabling the pass). **Types are reused, not stored**: from the runtime value's reified
   `TypeRepr` tag and, via `def_span`, from the checker's `expr_types`. The pc-scoped location-list variant
   (for optimized-build debugging) is deferred.
3. **DAP wire format = hand-rolled over tokio stdio**, mirroring `noeta-lsp::run_stdio`. DAP is
   `Content-Length`-framed JSON, same envelope as LSP. No DAP framework crate exists in the tree; a ~150-line
   hand-rolled protocol layer mirrors the working LSP loop and gives full control over DAP's fussy
   request/event ordering, with no new dependency. *(Reversible: swap in the crates.io `dap` crate at D0.)*
4. **Execution runs on its own thread; the adapter thread parks it at breakpoints.** The VM runs
   synchronously and pauses by *blocking* inside the dispatch-loop hook. The adapter spawns the program on a
   worker thread and talks to it over channels: `run/continue/step` commands in, `stopped/output/terminated`
   events out. The request loop stays responsive while the program is parked. Core new architecture; lands in
   D0/D1.
5. **Transport = stdio only** (as LSP). TCP is a trivial later add.
6. **`noeta-dap` is a new crate + a `noeta dap` CLI subcommand.** Mirrors `noeta-lsp`: `cmd_dap()` →
   `noeta_dap::run_stdio()`. Depends on `noeta-loader`, `noeta-check`, `noeta-compiler`, `noeta-bytecode`,
   `noeta-vm`, `noeta-runtime`, `noeta-backend`, `noeta-span`, `noeta-diagnostics`, `tokio` (the set `run`
   uses, plus tokio). Keeps the debug-adapter weight out of the CLI except at the entry point.
7. **Line/column mapping reuses `noeta-span`.** `Source::line_col` is already 1-based (DAP is 1-based) and
   `line_starts` gives the cheap line→byte reverse direction. Start line-granular; column negotiation deferred.

---

## Architecture — where the pieces live

```
editor debug UI  ──DAP/stdio (Content-Length JSON)──►  noeta dap  (noeta-cli Command::Dap)
                                                             │
                                                             ▼
                                                     noeta-dap crate
                        ┌────────────────────────────────────────────────────┐
                        │ Adapter (request loop, on the stdio thread)         │
                        │   • decode DAP requests → commands                  │
                        │   • encode events (stopped/output/terminated)       │
                        └───────────────┬────────────────────────────────────┘
                                        │  cmd channel ▲ event channel
                                        ▼             │
                        ┌────────────────────────────────────────────────────┐
                        │ Debug session (worker thread)                       │
                        │   • compile debug (JIT off, coalesce off, debug-info)│
                        │   • run the VM with a DebugHook consulted in dispatch│
                        │   • breakpoint set + step-mode; parks by blocking    │
                        └───────────────┬────────────────────────────────────┘
                                        ▼
                          noeta-vm  (tier-0)  ·  noeta-compiler (debug-info side-table)
              explicit Frame stack (proto/pc/base) · per-op spans · SourceMap   (mostly built)
```

New state the debugger owns: **a `DebugHook`** (breakpoints + step-mode) consulted inside the VM dispatch
loop; **a command/event channel pair** between the request loop and the worker thread; **the `SourceMap`**
(from the loader) for line↔span resolution; and **the debug-info side-tables** on the compiled `Module`.

---

## What already exists vs. what we build

**Reused as-is (free / near-free):** the production VM and its explicit `Frame` stack (proto/pc/register
window); per-op source `spans`; `SourceMap` line↔span resolution (`Source::line_col`, `line_starts`); the
loader's multi-file `SourceId`⇄path mapping; the whole `run` compile pipeline (`load` → `check_all` →
`compile_real`); the tier-0-only run shape (`VmBackend::run_module`, `jit=false`); the `noeta-lsp` stdio/tokio
server shape to copy; the compiler's existing `methods`/`destructors` reverse maps and its `frame_locals`
pin-through-coalescing path; **variable types** from the runtime value's reified `TypeRepr` and the checker's
`expr_types` (joined on `def_span`); `noeta-check` for validating `evaluate` expressions (D5).

**New plumbing (the real work), smallest→largest:**

- A **line→pc reverse index** for `setBreakpoints` (one-pass scan per prototype). *Small.*
- **Compiler debug-info side-tables** — `LocalDebug { name, reg, def_span }` at `declare_local` and
  `proto → { name, def_span }` at `declare_fn`, gated behind a debug-compile flag; skip `coalesce` in debug
  mode. Attached to `Chunk`/`Module`, read only by the debugger. *Medium — the load-bearing compiler change.*
- A **`DebugHook` consult-site inside the VM `dispatch` loop** (`lib.rs:2152`) — breakpoint check + step
  predicate, reading the live `Frame` (proto/pc → span → line) and parking the worker thread. Gated so it is
  free when unattached. *Medium — the load-bearing VM change.*
- The **worker-thread + channel run architecture** (decision 4) so pausing = blocking without freezing the
  adapter, plus a **debug-run entry** that compiles with debug-info + JIT off. *Medium.*
- The **DAP protocol layer** (framing, request/response/event envelopes, capability handshake). *Medium, but
  mechanical — mirrors the LSP layer.*

---

## Slices

One editor-visible capability per slice, each independently demonstrable. Like the LSP, this is I/O-facing,
so the differential/leak oracles don't apply directly; each slice is tested with **in-process DAP
request/response + event fixtures** (drive the adapter, feed a program + a request sequence, assert the
emitted events), plus unit tests for the line↔pc index and the debug-info emission (compile a snippet, assert
the `reg→name` / `proto→name` tables).

| # | Slice | Delivers | Notes |
|---|-------|----------|-------|
| **D0** | Adapter skeleton + launch/terminate | An editor can start a debug session that runs a program to completion | `noeta-dap` crate + `noeta dap` subcommand; stdio `Content-Length` framing; `initialize`→capabilities, `launch`, `configurationDone`; program compiled **JIT-off** and run on the worker thread; `output` events stream stdout; `terminated`/`exited` on completion. **Wire-format + threading + tier-0 decisions land here.** No breakpoints yet. |
| **D1** | Debug-info + breakpoints + stop-on-entry | Program pauses at a red-dot line | Compiler debug-info side-tables behind a debug flag (emit `frame_locals` unconditionally → named locals pinned through coalescing → clean 1:1 `reg→name`); line→pc index; `setBreakpoints(source, lines)`; the `DebugHook` consult inside `dispatch` parks the worker and emits `stopped(reason: breakpoint)`. `stopOnEntry`. `continue` resumes. **The load-bearing compiler + VM changes land here.** |
| **D2** | Call stack + scopes + variables | The paused state is inspectable | `stackTrace` from the live VM `Frame` stack + `proto→{name,def_span}` (name + file:line per frame); `scopes` (Locals; `self`/fields when in a method); `variables` reads each frame's register window through the `reg→name` table → DAP `Variable{name, value, type}` (value via the VM's value display; type from the runtime value or checker). |
| **D3** | Stepping | Step over / into / out | Extend `DebugHook` with a step-mode over the frame stack: `next` (stop at next line, same frame depth), `stepIn` (any deeper), `stepOut` (shallower), using pc→span line-change + `frames.len()` depth. Each emits `stopped(reason: step)`. |
| **D4** | Wire the VS Code / VSCodium client | F5 launches the debugger | Add a `DebugAdapterDescriptorFactory` + `launch.json` contribution to `editors/vscode-noeta/` spawning `noeta dap`. Mirrors the existing LSP client wiring. Works in VSCodium (built-in DAP UI, no Marketplace dep). |
| **D5** ✅ | Evaluate — **variable paths** (watch / hover / repl) | A name, `.field`, `[index]` against a paused frame | `evaluate(expr, frameId, context)`: parse the expression and resolve it read-only against the paused frame's live register window (a `Resume::Evaluate` serviced inside the pause loop on the run worker, where the values live). `supportsEvaluateForHovers` advertised. A resolution failure (unknown name, out of bounds) returns a clean error, so a hover shows nothing rather than a wrong value. |
| **D5.1** ✅ | Evaluate — **operators** (arithmetic / comparison / logical) | `x + 1`, `p.x > p.y`, `xs[i] + 1`, `a && b` | Extend the resolver with `Binary` / `Unary` via the VM's own `apply_binary` / `apply_unary` (so the semantics match a real run), literal leaves, computed indices, and `&&`/`||` short-circuit. Still **side-effect-free** (no calls), so it is safe for hover too — no VM run-loop change. A **call** (`xs.len()`, `f(x)`) returns a clean error pointing at D5.2. |
| **D5.2** ✅ | Evaluate — **calls** (user functions/methods) | `xs.len()`, `f(x)`, `p.mag()`, and compositions | The debugger returns the parsed expression to the dispatch loop as `DebugAction::Evaluate` (the trampoline); the loop, holding `&mut Vm` with the debugger lifted *out* of `self` (so a call never re-breaks), runs `Vm::debug_eval` — an owned-RC AST walker that dispatches methods via `run_method_handle` and functions via `call_value`, the *same* machinery a real call uses. Hover refuses calls (`allow_calls=false`); watch/repl allow them. |

---

## D5.2 — evaluating **calls** (as shipped)

D5/D5.1 answered variable paths and operators by reading the live register window and applying the VM's own
`apply_binary`/`apply_unary` on the run worker — side-effect-free, correct for hover, and needing no VM
run-loop change. Calls (`xs.len()`, `f(x)`, `p.mag()`) had to *run* code in the program's context, which
the pause point could not do, because the debugger is consulted **inside** the dispatch loop, holding a
read-only `DebugView`, not `&mut Vm`. The shipped design is two pieces — and it turned out **simpler than
the original sketch**, which is preserved after it for the record.

1. **The `DebugAction::Evaluate` trampoline.** `DebugAction` gained an `Evaluate(DebugEvalRequest)` variant
   carrying the parsed expression, the target frame, an `allow_calls` flag (false for a hover), and a
   reply channel. On an evaluate the paused debugger *returns* it instead of handling it in place. The
   dispatch loop, at the consult site, **holds the debugger `Box` out of `self` for the whole pause** —
   which both frees `&mut self` for the evaluator *and* auto-disarms the nested run's own debug consults
   (`self.debugger` is `None` while paused, so `f(x)` never breaks inside `f`). It runs
   `Vm::debug_eval_request`, sends the reply, and loops; `before_op` sees a `mid_pause` latch and resumes
   waiting without re-announcing the stop. One match arm, no frame-stack surgery, no `unsafe`.

2. **A direct owned-RC AST walker — no fragment compilation.** `Vm::debug_eval` walks the `Expr` on the
   live heap, returning a freshly *owned* value at each rung (retain what it reads, release what it
   consumes — the discipline the interpreter uses everywhere, needed because this runs against a paused
   program's real heap). Names resolve against the frame's captured locals then `Module::global_names`;
   a **method** call dispatches through `run_method_handle("", name, …)` (which already handles both user
   methods *and* built-ins like `len`); a **function** call through `call_value`. Both are the exact
   primitives a real call uses, so a watch and a run agree by construction. A watch's transient
   diagnostics/abort-trace are rolled back so a failed watch never pollutes the debugged run.

**Why no `SessionCompiler` / module extension (the sketch's hard part) was needed.** The sketch assumed the
expression had to be *compiled* into a synthetic prototype and the running module *extended* with it —
which drags in checkerless stable-id accumulation to resolve globals/methods against the running module's
id spaces. But a watch expression only ever *calls things that already exist*: dispatching **by name at
runtime** (`global_names` lookup, the `(type, method)` method table) sidesteps compilation entirely. So the
deliverable is met by a ~120-line evaluator and one trampoline arm, with zero new compiler machinery. The
limit of this approach is that an expression needing *new* code — e.g. a closure literal argument
`xs.map(fn(x) => x*2)` — is not supported (it returns a clean "cannot evaluate this form" error); that, and
assignment / side-effecting statements, stay deferred. The by-name approach covers the D5.2 targets
(`xs.len()`, `f(x)`, `p.mag()`) and their compositions (`twice(p.mag2()) + 1`).

### Original sketch (superseded — kept for the record)

> Bind the frame's locals via the ordinary call protocol: compile the expression as a synthetic function
> whose *parameters* are the in-scope local names, and call it with the register-window values. The hard
> part is **module extension** — the fragment must resolve globals/functions/method tables against the
> *running* module's id spaces (a one-shot checked `compile()`), so compile debug-eval fragments
> *checkerless*, reusing the REPL's `SessionCompiler`-style accumulation seeded from the running module.
> — Not built: runtime by-name dispatch made compilation unnecessary.

---

## Deferred (revisit after the arc)

- **DWARF-grade location lists** — coalesce named locals too (not just temporaries) and remap the debug-info
  into `(reg, pc-range)` ranges, so even a fully-optimized build stays debuggable. Tighter debug frames + the
  ability to debug production-optimized code; more complex emission. (This milestone instead pins named locals
  through coalescing, which is source-faithful and far simpler.)
- **Debugging `isolate` parallelism** — stepping across real inter-isolate OS threads; start with the main
  isolate / sequential + single-thread-async code.
- **Conditional / hit-count / logpoint breakpoints**, **data/exception breakpoints**, **`setVariable`**
  (mutate a local from the Variables view), **`setExpression`**.
- **Column-precise breakpoints / UTF-16 column negotiation** — start line-granular, 1-based.
- **Sub-expression / sub-statement stepping** — statement/line granularity first.
- **Debugging inside JIT'd regions** — out of scope by construction; debug sessions pin tier-0. (A far-future
  option is JIT-emitted debug info, but tier-0 is the right answer for a debugger.)
- ~~**Stripping the tree-walker from the production binary**~~ — ✅ done: the REPL moved onto the VM and
  `noeta-eval` is out of `noeta-cli` (the REPL-on-VM arc, merged to `main`). The tree-walker now lives only
  in `noeta-conformance` as the differential oracle.
- **Reverse debugging / time-travel**, **post-mortem / core-dump inspection**, **Marketplace publishing**.

---

## Gate — this milestone is done when

`noeta dap` starts over stdio, a VS Code/VSCodium (or any DAP) client launches a `.noe` program under it
(compiled JIT-off with debug info), and the developer can: set a line breakpoint and have execution stop
there; see the call stack with function names and source lines; expand a frame and read its named local
variables' values; and step over / into / out — each covered by in-process request/response + event fixtures,
with the workspace clean under fmt/clippy and zero new `unsafe`. Evaluate (D5) is a stretch, not a gate
condition.
