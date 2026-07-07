# Tooling unification — one engine under REPL, DAP, and LSP

**Status: ARC COMPLETE (T0–T7), branch `tooling-unification`.** Motivated by a real smell: the
tooling had been accreting parallel machinery — most recently the DAP's D5.2 watch evaluator, the
*third* thing in the tree that evaluated expressions. This arc de-duplicated what had actually
diverged, and landed the one big unification: **the debug console is a REPL over the paused
program** (session-compiled fragments, closures included, escapes surviving resume) — which
*deleted* an evaluator instead of growing one. The tree is back to exactly two expression
evaluators: the production VM and the conformance oracle.

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

2. **Module swap through an arena** (supersedes the overlay sketched at planning) — the module is
   borrowed (`&'m Module`) and cannot grow, but every extended snapshot the session compiler
   produces is a **stable-prefix superset** of every earlier one. So instead of overlaying tables,
   `Vm::install_fragment` (called with `&mut Vm` at the trampoline) swaps `self.module` to the
   extended snapshot, kept alive in a `typed_arena::Arena<Module>` the debug driver owns
   (`DebugSession<'m>`). Old frames keep executing byte-identical code (the prefix); new frames —
   fragment entries, escaped closures, fragment-type destructors — resolve *everything* (protos,
   interned names, global names, cache slots, reflection) against the newest module with **zero
   per-site changes**. The whole hot-path delta: the dispatch loop re-reads `self.module` per frame
   transfer (`'reload`) instead of once per dispatch, plus one cache-array growth check there.
   One wrinkle: `extend()` rewrites proto 0 per entry, but running frame 0 *is* proto 0 — the
   install relocates the fragment entry to a fresh top index and restores `main` at 0. Derived Vm
   tables (interned shapes/schemas, methods, destructors, defaults, derives, reachability, globals)
   grow by the same appends `load`/`sync_to` perform; `&'static` shape interning makes identity
   hold by construction. **Perf gate:** pinned interleaved A/B on `vm_recursion/fib` (maximal
   `'reload` frequency) + `vm_dispatch/loop_sum`; the JIT interlock is asserted (debug = tier-0).

   *Why the overlay died:* enumerating its consult sites found ~50 scattered lookups — every
   `protos[i]`, interned-name, global-name, and cache-slot read, each a missed-site bug waiting.
   The swap gets the same soundness from the T3 prefix-stability invariant with two localized
   edits, and fragment code needs no operand-table translation at all.

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
| **T0** ✅ | Merge + baseline | `debug-adapter-d5` → `main` (real merge — http + JIT-throughput arcs had landed); branch `tooling-unification` | Full workspace (73 suites incl. conformance) green before anything else. |
| **T1** ✅ | Small unifications | One spelling for types; one fragment parser; one render loop | `Value::type_display()` (3 verbatim sites); `noeta_parser::parse_fragment` + `Fragment::trailing_expr` (REPL `:type` + DAP); `noeta_diagnostics::render_mapped` (CLI + DAP). |
| **T2** ✅ | `Sites` bundle | `compile_with_sites(program, Sites, real_isolates, debug)` — was 15 args | `Sites` in noeta-check (11 maps + destructor relevance); `Checked { diagnostics, expr_types, sites }`; `reference_run(program, Sites)`; noeta-db's mirror embeds it; all ~9 unpacking call sites collapsed. New site map = 1 field + the consumers that care; the differential catches a forgotten semantically-relevant map. |
| **T3** ✅ | Session from a checked compile | `compile_with_sites_session` (keeps the `ModuleCompiler` alive) + `VmSession::adopted` | Shared `compile_to_mc` core; production `compile_inner` still MOVES its tables (no clone tax). Tests: fragment calls checked fns/globals by original ids; `Rc<Shape>`→`&'static Shape` identity across entries via structural equality; fragment closure over checked ids; stable name prefixes. `VmSession::adopted` also enables a future `noeta repl --load`. |
| **T4** ✅ | Module swap + arena (superseded the overlay — see architecture §2) | `Vm::install_fragment`: session-extend → relocate entry (proto 0 stays `main`) → grow derived tables → swap `self.module` to the arena'd snapshot | Dispatch re-reads the module per `'reload` + cache-array growth check; JIT interlock asserted. **Bench (pinned interleaved A/B, taskset -c 2):** `vm_recursion/fib` medians t4 −5.1%/−1.2% (fib/20/24, 8 rounds — frame-transfer-heavy, the change executes per call); `vm_dispatch/loop_sum` +0.9–1.3% with overlapping spreads (structurally executes the change once per run → layout luck). Verdict: noise, no regression. |
| **T5** ✅ | Console = session fragments | Closures at the console; statements; escapes survive resume | Adapter parses (SourceId `u32::MAX` — no span collisions); VM wraps the fragment as a closure whose params are the frame's in-scope locals, binds it to a NUL sentinel global, calls it with the paused window's values; trailing expr → `return`. `run_module_debug_session` owns the arena. Test: `xs.filter(fn(x) => x > 15)`; closure composing frame local + program fn; **escaped closure rebound into a program global, called by the program's own code after `continue`** (→ 17). |
| **T6** ✅ | Hover purity + walker deletion | `is_pure_expr` AST gate + `Vm::pure_eval` runtime backstop; the D5.2 walker deleted (~260 lines) | Backstop is ONE chokepoint: every way of running user code pushes a frame, every frame push re-enters `'reload` — a pure run refuses there (decides object-`Index` / user-ordering, which the AST cannot). Evaluator count back to 2 (VM + oracle). Test: `b[0]` with an `Index` impl runs in a watch, refused on hover; `b.xs[1]` still hovers. |
| **T7** ✅ | Docs + plans sweep | Aligned docs | plans/debug-adapter (D5.2 walker superseded by the fragment engine), this doc, memory. |

Gate (met per slice): full conformance suite (differential, leaks, jit-diff, session parity) + all
DAP/LSP/VM tests green; the T4 bench attached to its commit; zero new `unsafe`.

---

## Deferred (explicit, pending confirmation)

- **Persistent console bindings** (`mut x = 1` across console entries): needs a frame-scope/module-
  scope hybrid (a fragment's `mut` is a closure-body local today); design constraints noted above,
  not started.
- **`setVariable` / assignment to frame locals from the console** (`x = 5` mutating a frame
  register): frame locals pass into fragments BY VALUE, so assigning one mutates the fragment's
  copy — the register write-back is the DAP-protocol-shaped sibling; rides the same fragment
  machinery once wanted.
- **Watch-memoization**: each evaluate appends one small proto + global slot to the session; a
  watch panel re-evaluates on every step, so a long session accumulates them. Memoize fragments by
  (text, in-scope param names) → entry proto (indices stay valid — the module only grows).
- **SessionChecker** (incremental type-checking at the REPL/console prompt): planned in
  plans/repl-on-vm, orthogonal to this arc; fragments stay checkerless here.
- **REPL cosmetic convergence**: the REPL already *is* the session engine; beyond T1's shared
  helpers there is nothing structural to unify.
