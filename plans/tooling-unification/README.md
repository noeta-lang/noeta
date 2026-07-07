# Tooling unification — one engine under REPL, DAP, and LSP

**Status: PLANNED (recon complete, slices not started).** Motivated by a real smell: the tooling has
been accreting parallel machinery — most recently the DAP's D5.2 watch evaluator, the *third* thing in
the tree that evaluates expressions. This arc de-duplicates what has actually diverged, and lands the
one big unification: **the debug console becomes a REPL over the paused program** (session-compiled
fragments, closures included), which *deletes* an evaluator instead of growing one.

This document is reconnaissance-backed: three parallel sweeps mapped (1) every pipeline driver,
(2) the session/VM ownership model, (3) cross-tool helper duplication. Findings are inlined with
file:line so the slices below argue from verified facts.

---

## Recon findings — what is actually duplicated (and what is not)

### Genuine duplication (fix)

| # | What | Where | Size / risk |
|---|------|-------|-------------|
| 1 | **Three expression evaluators.** Production VM; the tree-walker oracle (*intentional* — it is the differential); and `Vm::debug_eval` (D5.2's AST walker for watch expressions). The walker re-derives name resolution, dispatch order, and arity behavior by discipline. | `noeta-vm/src/lib.rs` (`debug_eval`, `debug_eval_call`) | **The big one.** Every language feature added must be mirrored into the walker or watches silently lag the language. |
| 2 | **The 12-argument `compile_with_sites` signature**, hand-unpacked at ~9 call sites: CLI `compile_real` (main.rs:226), DAP `compile_checked` (session.rs:141), noeta-db `from_check_output` (lib.rs:231), conformance (lib.rs:99, differential.rs:164/228, leaks ×2). | across 4 crates | Adding a checker site map = 9 edits. (Compile-time-caught, but 9 edits of ceremony each time.) `noeta-compiler` already depends on `noeta-check`, so a `Sites` bundle struct has no dep-direction problem. |
| 3 | **Type rendering triple**: `value.reflect().map(|t| t.to_string()).unwrap_or_else(|| value.type_name()…)` verbatim in `noeta-vm/session.rs:226` (REPL `:type`), `noeta-dap/debugger.rs:385` (Variables view), `noeta-vm` `debug_eval_request` (D5.2). | 3 sites | Small, but it is the *user-visible type spelling* — divergence here is exactly the "IDE shows one thing, debugger another" failure. |
| 4 | **Fragment parsing twins**: REPL `:type` (`main.rs:1325`) and DAP `parse_expr` (`debugger.rs:406`) both do Source(`"{expr};"`) → lex → parse → extract; REPL entry parsing adds a retry-with-`;`. | 2–3 sites | Behavior can drift (e.g. only one gets the retry). |
| 5 | **Diagnostics-render loops**: CLI `emit_diagnostics_mapped` (main.rs:305) vs DAP `render_all` (session.rs:175) — same loop, stderr vs String. | 2 sites | Trivial; cheap to share. |

### Verified NON-problems (leave alone)

- **The tree-walker oracle "duplication" is the point** — it is the differential's independent
  implementation. Never unify.
- **LSP vs DAP position mapping** (0-based UTF-16 `offsets.rs` vs 1-based `line_col`) — correct
  per-protocol behavior, cleanly isolated.
- **LSP severity mapping** (lib.rs:669) — an *exhaustive* match, no wildcard: a new `Severity` variant
  breaks the LSP build. Self-aligning; no action.
- **Scope machinery**: LSP completion walks the AST (`resolve.rs:309`), DAP reads `debug_locals`
  bytecode metadata — different compilation stages, correct separation.
- **Protocol framing**: LSP delegates to tower-lsp; DAP hand-rolls ~80 lines. Different runtimes;
  not worth a shared crate.
- **Value display**: already one implementation (`Value::display`), shared by REPL echo, DAP
  variables, tracebacks.
- **Pipeline drivers overall**: thinner than feared — every driver calls the same stage functions;
  the salsa DB memoizes the same calls. The only real friction is finding #2's signature.

### Session/VM facts that make the console-REPL feasible (verified)

- **The heap is thread-local** (`noeta-value/heap.rs:33`): a fragment run on the run-worker thread
  shares the paused program's values natively.
- **`SessionCompiler.extend` is append-only** (compiler lib.rs:397): proto indices, global slots,
  shapes, interned names never renumber. Entry-N ids valid forever. Only proto 0 (the entry `main`)
  is rewritten per entry, and nothing live references it.
- **Shape identity is per-Vm `Rc<Shape>`, preserved across entries by seeding**
  (`SessionState.shapes` → `load_seeded`, session.rs:121): method dispatch, inline caches, and
  equality all key on the shape *pointer* — any fragment machinery must reuse the paused Vm's
  handles, never re-wrap.
- **Closures hold a raw `proto: u32`** (heap.rs:281) with **no runtime validity check**
  (lib.rs:6130): a closure is only callable under a module whose proto table contains its index —
  the constraint that shapes this whole design (see "the escape problem").
- **`Vm` owns everything except the module**: `globals`, `shapes`, `methods`, `type_reprs`,
  `packed_schemas`, host, executor, channels, reactive graph are owned fields, growable through
  `&mut self`. Only `module: &'m Module` is borrowed — the *one* thing a fragment cannot extend
  in place.

---

## The centerpiece: debug console = a REPL over the paused program

### Goal

`evaluate` in watch/console context compiles the fragment with the **same engine as the REPL**
(`SessionCompiler`, checkerless — the REPL's proven stance) instead of walking the AST. This buys:

- **Closures**: `xs.filter(fn(x) => x > 20)` works at the console — new code compiles into new protos.
- **Deletes evaluator #3**: the D5.2 walker goes away; watch semantics are *the compiler's semantics
  by construction*, forever aligned with the language.
- **One fragment story**: REPL entries and console fragments are the same kind of thing, differing
  only in scope (module scope vs a paused frame).

### The escape problem (why the naive design is unsound)

A console fragment can *store* a closure into program state — `effect(fn => …)` registers it in the
reactive graph; a callback set on a live object survives the fragment. If that closure's proto lives
only in a fragment-local module snapshot, the **resumed** program later calls
`self.module.protos[idx]` with an out-of-range or wrong index — silent corruption, no runtime check.
So fragment protos must remain resolvable **for the rest of the run**, in the *paused program's own
Vm*. Ephemeral per-fragment modules are ruled out; forbidding escape is undetectable-in-general and
was rejected.

### Architecture (three pieces)

1. **`SessionCompiler::adopt`** — seed a session from the debug launch's *checked* compile. Today
   `compile_with_sites` builds a `ModuleCompiler` internally and drops it; the DAP launch path keeps
   it alive and adopts it into a `SessionCompiler`, so `extend(fragment)` appends protos/names/
   globals/shapes onto the real program's id-spaces (stable-prefix, exactly like REPL entries).
   The initial compile stays fully checked; fragments are checkerless like REPL entries — an
   ill-typed fragment is a console error, not a crash.

2. **A VM overlay for runtime-appended code** — the module is borrowed (`&'m Module`) and cannot
   grow, but the trampoline (D5.2) holds `&mut Vm`. The Vm gains an overlay for exactly the tables
   that live in `Module`: `protos`, interned `names`, `global_names` (everything else the Vm already
   owns and can grow: `globals`, `shapes`, `methods`, `type_reprs`, `packed_schemas`). Lookups go
   through helpers — `if idx < module.protos.len() { &module.protos[idx] } else { &overlay… }` — a
   predicted-always-true branch on non-debug runs. **Perf gate:** pinned interleaved A/B bench on the
   call-heavy suite; the overlay is populated only on debug runs (JIT already off; assert the
   interlock). Escaped closures stay callable after resume because the overlay lives on the Vm for
   the rest of the run.

3. **Frame-scope binding via the ordinary call protocol** — the fragment compiles as a synthetic
   function whose parameters are the frame's in-scope local names (from `debug_locals`, `self`
   included for method frames), called with values read from the paused register window. No upvalue
   tricks; it is the REPL's sentinel idea in frame scope.

**Hover stays side-effect-free — via the VM, not a second evaluator.** Purity is not statically
decidable here (`o[i]` on a user object dispatches the `Index` trait's `get` — a call; checkerless,
the receiver's type is unknown until runtime). So hover compiles like any fragment but runs with a
`pure_eval` flag: the frame-pushing dispatch paths (`Op::Call*`, user-method `CallMethod`,
object-`Index`) refuse with a clean error instead of executing. Precise (only refuses what *would*
run code), sound, and it is what lets the walker be deleted rather than kept "for hover".

### Console scope decision

First slice supports **expressions** (with calls and closures) in frame scope. Persistent console
*bindings* (`mut x = 1` surviving across console entries) need module-scope compilation, which cannot
see frame locals — a hybrid design deferred as an explicit follow-on (see Deferred), pending
confirmation.

---

## Slices

| # | Slice | Delivers | Notes |
|---|-------|----------|-------|
| **T0** | Merge + baseline | `debug-adapter-d5` → `main`; branch `tooling-unification` | `main` has moved (JIT-throughput arc); real merge. Full conformance + dap + vm green before anything else. |
| **T1** | Small unifications | One spelling for types; one fragment parser; one render loop | `Value::type_display()` in noeta-value (3 sites); shared expression-fragment parse helper (REPL `:type` + DAP); `render_mapped` in noeta-diagnostics (CLI + DAP). |
| **T2** | `Sites` bundle | `compile_with_sites(program, &Sites, opts)` | Struct lives in noeta-check (compiler already depends on it); `Checked` exposes it; update all ~9 call sites. New site map = 1 field + the sites that care. |
| **T3** | `SessionCompiler::adopt` | A session seeded from a checked compile | Keep the launch `ModuleCompiler` alive; adopt; tests: post-adopt `extend` calls base fns/types, ids stable, shapes appended not re-wrapped. |
| **T4** | VM overlay | Runtime-appendable protos/names/global_names on `Vm` | Lookup helpers at proto/name resolution sites; JIT interlock assert; **pinned interleaved A/B bench** (call-heavy) — the branch must be noise-level. |
| **T5** | Console = session fragments | Closures at the console; walker's cases via compiled code | Trampoline: parse → `extend` (synthetic fn, params = frame locals) → append via overlay → call → render. DAP tests: closure in `map`/`filter`, **escaped closure via `effect` + resume + fire**, composition, error surfacing. |
| **T6** | Hover purity + walker deletion | `pure_eval` flag; `Vm::debug_eval`/`debug_eval_call`/`debug_index` deleted | Evaluator count back to 2 (VM + oracle). Hover tests: paths/operators still answer; call/object-index refused at runtime. Bench the flag check (fold into T4's run). |
| **T7** | Docs + plans sweep | Aligned docs | Update plans/debug-adapter (D5.2 walker superseded), plans/repl-on-vm cross-refs, this doc's status, memory. |

Gate: full conformance suite (differential, leaks, jit-diff, session parity) + all DAP/LSP/VM tests
green per slice; T4/T6 benches attached to their commits; zero new `unsafe`.

---

## Deferred (explicit, pending confirmation)

- **Persistent console bindings** (`mut x = 1` across console entries): needs a frame-scope/module-
  scope hybrid; design sketched above, not started.
- **`setVariable` / assignment from the console** (`x = 5` mutating a frame register): the
  DAP-protocol-shaped sibling; rides the same fragment machinery once wanted.
- **SessionChecker** (incremental type-checking at the REPL/console prompt): planned in
  plans/repl-on-vm, orthogonal to this arc; fragments stay checkerless here.
- **REPL cosmetic convergence**: the REPL already *is* the session engine; beyond T1's shared
  helpers there is nothing structural to unify.
