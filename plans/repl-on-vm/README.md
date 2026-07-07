# REPL on the VM — strip the oracle backend from production

**Status: planning + implementation (this branch).** The tree-walker (`noeta-eval`) is the differential
oracle: every program runs through it *and* the bytecode VM, and the two `RunResult`s are asserted
identical. That is exactly what we want in the **test** graph. But it is also, today, the **REPL**
backend — `noeta repl` drives `noeta_eval::Session` — which means the oracle crate ships in the
production `noeta-cli` binary for no reason other than that the REPL was built on it first (M0, before
the VM existed).

This arc moves the REPL onto the VM and cuts `noeta-eval` out of `noeta-cli`'s dependency graph. The
oracle stays alive where it belongs — in `noeta-conformance`, a `dev-dependency`/test-only crate — so we
lose nothing in test coverage and the shipped binary stops carrying a second execution engine.

## Why this is a clean cut

The dependency graph already isolates the work. `noeta-cli` uses `noeta-eval` for **exactly one thing**:
`Session` / `SessionOutput` in `cmd_repl`. The only other dependent of `noeta-eval` is
`noeta-conformance` (test-only). So the REPL is the *sole* production consumer of the oracle backend;
once it moves, stripping is a one-line delete in `noeta-cli/Cargo.toml`. No feature gate, no `cfg`
sprawl.

## The one hard problem: cross-entry object identity

A REPL session keeps live heap values between entries — `mut x = Res.new()` in entry 1, `x.method()` in
entry 2. Those values are refcounted heap allocations that **cannot be recompiled**. Two facts about how
the VM represents them decide the whole design:

- A **closure** value stores a raw `proto: u32` index (`Value::closure(proto, upvalues)`).
- An aggregate value carries an `Rc<Shape>`, and **shape identity is a pointer comparison** (the load
  comment: "equal-built aggregates point at one shape"). Inline caches and structural-eq fast paths key
  on that pointer.

This rules out the tempting simple model — *recompile the accumulated source from scratch each entry,
execute only the new statements*. A fresh compile reassigns proto indices and rebuilds the shape table,
so a closure or object created in entry 1 would carry a `proto`/`Rc<Shape>` that means something
different (or nothing) in entry 2's module. Silent dispatch corruption. **Rejected.**

The sound model is **stable-id accumulation**: one module and one runtime that both **grow by append**,
never rebuild. Proto indices, global slots, shapes, and method-table entries assigned in entry *N* stay
valid forever. This is not a big lift — `intern_global` / `intern_name` are already append-only dedup;
the work is *keeping the compiler's tables alive across entries* instead of consuming them into a
finished `Module`.

## Architecture decision: keep `Vm<'m>` and the hot path untouched

Reconnaissance (this branch, before writing code) established two load-bearing facts:

1. **Inline caches are loop-local.** `run()` rebuilds `caches: Vec<Option<(Rc<Shape>, u32)>>` fresh on
   every call (`noeta-vm/src/lib.rs`, in `run`). They are *not* session state — a new entry simply
   starts with cold caches, which is correct and free.
2. **The `'m` lifetime is only ~7 sites**, and `noeta-jit` does **not** store the module (it holds a
   Cranelift `JITModule`, keyed by proto index). So dropping `'m` and making `Vm` own an `Arc<Module>`
   *is* tractable — **but** it would change the universal, per-op `self.module` access on the hot path
   we just proved zero-cost (production stack traces arc). Not worth the risk for a cold REPL feature.

**Chosen shape: an ephemeral `Vm<'m>` per entry over a persistent `SessionState`.** All session
complexity lives in a new `session` module in `noeta-vm`; the `Vm` struct, the dispatch loop, and the
JIT are byte-for-byte unchanged.

```
VmSession (owns, persists across entries)
├── compiler:  SessionCompiler   — the accumulating compile tables (was ModuleCompiler, kept alive)
├── module:    Module            — reassembled from the compiler each entry (proto 0 = this entry's main)
└── state:     SessionState      — the persistent runtime + derived tables
      ├── runtime:  globals, global_order, next_id, channels, channel_progress, reactive, host, executor
      └── derived:  shapes, packed_schemas, type_reprs, map_packed, methods, destructors,
                    field_defaults, destruct_reachable, comparable_derives, tojson_derives
```

The derived tables are a pure function of the module and grow by append **in lockstep** with it. The
crucial invariant: **entry-N shapes keep their `Rc<Shape>` identity forever** — the session *appends*
new shapes for a grown module tail, it never rebuilds index `0..old_len`. (`Vm::load` today rebuilds
*all* shapes; the session path must not.)

Per entry, `VmSession::eval`:

1. `compiler.extend(entry_program)` — register this entry's new types/globals/methods into the
   persistent tables, compile new methods/fns into **appended** protos (stable indices ≥ old len),
   compile the entry's top-level statements into **proto 0** (overwriting the previous entry's dead
   main — safe, because no live value references proto 0). Reassemble `module`.
2. `state.sync_to(&module)` — append derived-table entries for the module's new tail, preserving old
   `Rc` identity.
3. Build an ephemeral `Vm::load_seeded(&module, state-fields)` that **plugs in** the persistent runtime
   + derived tables instead of rebuilding, run **proto 0** against the persistent globals via
   `Vm::run_top` (**no teardown**), then `Vm::into_state()` extracts the fields back into `state`.

At `:reset` / session exit, `Vm::teardown` runs once — reactive-clear, destroy globals in reverse order,
backup cycle collection — bringing leak residency to zero (the existing machinery, invoked once instead
of per-run).

The JIT's "globals never reallocate mid-run" invariant is **per-run** (`globals_ptr` is captured inside
`run`), so growing the globals vec *between* entries is safe by construction. The REPL runs **JIT-off**
regardless — entry chunks are cold, and it removes one variable during bring-up.

## What maps over cleanly

- **Trailing-expression echo** (`1 + 2` → `3`): the eval session's `rewrite_trailing_expr` sentinel
  trick is pure AST surgery, backend-agnostic — reused verbatim. It rewrites a trailing bare expression
  to `mut <sentinel> = expr;`; after the run we read the sentinel's **global slot**, display it, then
  release + unbind it (same "never let the sentinel pin a refcount across entries" rule the eval session
  documents).
- **`:drop <name>`**: replace the name's global slot with `Value::unbound()` and run the VM's value
  destruction — the exact per-slot operation `teardown` already performs.
- **`:bindings`**: `SessionCompiler.global_names` minus the prelude slots minus the sentinel.
- **`:type <expr>`**: gets *better* — `value.reflect()` + `TypeRepr`'s `Display` (production-stack-traces
  arc) gives `List<int>` with reified generics, where eval's `describe_type` erases to head constructors.
- **Traces**: `run_module_traced` already exists; the per-entry `abort_trace` / `call_sites` clearing
  discipline transfers directly (first-abort-wins would otherwise let entry 1's panic mask entry 2's).

## The semantic decision: the checker (checkerless first)

Today's REPL is deliberately **unchecked**. `noeta_eval::Session` lowers with `insert_drops(ir, None)`
(conservative destructor relevance) and runs **no checker across entries** — so there are no type errors
at the prompt, and rebinding `x` to a different type across entries is legal. The VM session **matches
this exactly**: it compiles from IR built *without* checker site-maps (no `type_of`/packed-list/handle
optimizations, conservative drops), so the migration is verifiable as a **pure backend swap** — same
observable behavior, different engine. Type-checking at the prompt is a real feature, planned below as
its own decision, **not** smuggled into this refactor.

## Slices (each green + committed)

- **R0 — split run / teardown.** Extract `Vm::run_top` (run main, release the returned value, release
  open `concurrent` scopes + JIT cache pins) and `Vm::teardown(mode) -> RunResult` (pre-teardown cycle
  collect, drain channels, reactive-clear, destroy globals in reverse order, backup collect, join
  isolates) from `run_and_teardown`, **behavior-identical** (same statements, same order). Prove
  re-entrancy: a test that loads once, runs two `run_top`s sharing globals, and asserts one `teardown`
  zeroes residency. Differential + leak oracle green. *(Pure refactor — no session code yet.)*

- **R1 — incremental compiler + `VmSession::eval`.** `SessionCompiler` (the `ModuleCompiler` tables kept
  alive, `extend` appends), `SessionState` + `Vm::load_seeded` / `Vm::into_state`, the derived-table
  `sync_to`, and `VmSession::eval` with the sentinel echo. Checkerless. Persist bindings, `fn`, `type`,
  `enum`, `class`, and `next_id()` continuity across entries.

- **R2 — meta-commands + session differential.** `:type` / `:drop` / `:bindings` / `:reset` on
  `VmSession`. A **session differential** in `noeta-conformance`: a script of REPL entries (redefinitions,
  cross-entry closures, destructor-bearing bindings, `:drop`, a mid-session panic) fed to **both**
  `noeta_eval::Session` and `VmSession`, comparing per-entry `SessionOutput` (stdout, echoed value,
  diagnostics, trace story). The program-level differential never exercised *persistent state across
  batches* — this closes a real coverage gap and stays forever as the REPL-semantics oracle, even after
  the CLI flip.

- **R3 — flip + strip.** Point `cmd_repl` at `VmSession`; delete `noeta-eval` from `noeta-cli/Cargo.toml`;
  verify the release binary builds with the oracle crate absent from its graph. Oracle remains only in
  `noeta-conformance`.

## Follow-on upgrades (planned, post-R3)

Each is cheap *once the REPL is on the VM* and currently impossible or awkward:

1. **Per-entry `SourceId`s.** A persistent session `SourceMap` with one `SourceId` per entry, so a trace
   into an *earlier* entry renders with real file/line instead of the name-only degradation we hardened
   `render_trace` for (the REPL currently reuses `SourceId::FIRST` across entries; an old-entry span
   points into text the current entry no longer has). On the VM this is natural: the `line_table` already
   carries per-statement spans; we just stop reusing one id.

2. **A `RealHost` REPL.** Today the REPL is sandbox-only (deterministic fs / logical clock / seeded PRNG,
   inherited from the eval session). On the VM we can pair the session with the CLI's `RealHost` +
   wall-clock executor (the `Host`/`Executor` seams already exist, M2), so `fs.open`, `time.now`, `uuid()`
   work at the prompt against the real machine — matching `noeta run`. Gate behind a `--sandbox` flag if
   we want the deterministic prompt back.

3. **Accumulated reflection.** The eval session rebuilds reflection **per batch** (`reflect::build`), so
   `attributes_of` / `roles_of` on a type declared in an *earlier* entry currently fails. The session
   differential (R2) will make us match that quirk first; then we can fix it on both backends
   (accumulate the reflection manifest across entries) or knowingly accept divergence. On the VM the
   `Module.reflection` is already a table we can grow by append.

4. **JIT at the prompt (optional).** Long REPL sessions with a hot loop could arm the JIT per entry. The
   catch is the module-swap-invalidates-compiled-code problem (§ architecture); deferred until there is
   demand, and only worth it for genuinely long-lived sessions.

## Incremental type-checking at the prompt (planned — its own arc)

This is the one genuine **semantics change** and therefore its own decision, sequenced *after* R3 so the
checkerless swap is proven first.

**What it buys.** Type errors *before* you hit enter on a mistake (E0022 missing signature, E0007 type
mismatch, E0048 missing return, …); the site-map optimizations the checker feeds codegen (packed lists,
`type_of` full fidelity, method handles) active in the REPL; `:type` reporting the *static* type, not
just the runtime one.

**What it costs.** Type errors appear at the prompt (today's REPL never rejects a well-parsed entry);
cross-entry retyping becomes restricted (rebinding `x: int` then `x = "s"` would error unless the entry
re-declares `mut x`); this is a behavior change users will feel, so it is opt-in until it's the default.

**Design sketch.** `noeta-check` currently exposes `check_all(program)` — whole-program, stateless. The
REPL needs a **persistent typing environment** accumulated across entries:

- A `SessionChecker` holding the cross-entry environment: the type of every live global binding, every
  declared `type`/`enum`/`class` signature, every top-level `fn` signature, and the coherence/trait
  registry. Entry *N* type-checks its statements against this environment, then commits its new bindings
  and declarations into it.
- Rebinding semantics: **resolved — no REPL-specific policy needed.** The language already allows what a
  REPL wants, verified against the checker (merged main): re-`mut x` re-declares (even with a *different*
  type — `mut x = 1; mut x = "hi"` is legal), a bare `x = expr` reassigns and flow-updates the type, a
  nested `mut x` lexically shadows, and redefining a `struct`/`class` is allowed. The *only* no-shadow
  rules are the reserved names: prelude value names (E0046 — `Ok`/`Err`/`some`/`none`/`panic`/`assert`)
  and native type names (E0049 — `Uuid`/`FileHandle`/`Iterator`/`Future`/`Sender`/`Receiver`/`Signal`/
  `Computed`/`Effect`). So the `SessionChecker` just runs the language's own rules per entry against the
  accumulated environment — no special REPL rebinding rule, and re-defining a binding at the prompt is
  not "shadowing the language forbids", it is exactly what the language already does. (Sharp edge, same
  as the checkerless REPL today: an entry that retypes `x` can leave a function compiled in an earlier
  entry against the old type stale — inherent to REPL redefinition, not caught by a per-entry check.)
- The checker's site maps (`type_of_sites`, `packed_list_sites`, `handle_sites`, …) become **per-entry**
  outputs threaded into that entry's `compile_with_sites` (the machinery already exists — the CLI/salsa
  path threads exactly these), so the REPL gets the optimized codegen the checkerless path forgoes.
- Diagnostics with a **session-relative** span: an error in entry *N* renders against entry *N*'s source
  (dovetails with follow-on #1, per-entry `SourceId`s — build these together).

**Sequencing.** Land R0–R3 (checkerless, differential-proven). Then build `SessionChecker` behind an
opt-in (`:check on` meta-command or a `--check` flag), differential the *checked* session against a
whole-program re-check of the accumulated source as its oracle, and only make it the default once it is
proven and the rebinding UX is settled with the user.

## Risks / honest costs

- **Deferred cleanup.** `:reset` and session exit become the moments garbage cycles are reclaimed, so a
  long-lived session holds its cycles until then — identical to today's eval session, worth keeping true.
  The backup cycle collector *could* run per-entry if this ever bites; not by default (it's O(live heap)).
- **Type redefinition.** Redefining a `type`/`class` mid-session creates a new shape; old instances keep
  their old shape but method dispatch (by name) resolves to the *newest* proto. This matches the eval
  session (reflection rebuilt per batch dispatches by name too), so the R2 differential pins it by
  construction — but it is a sharp edge to document for the eventual incremental checker.
