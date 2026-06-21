# Slice M1.1 — Salsa db plumbing

Status: done

> **Sequencing note (M1.0 retro):** the VM compile path is a single `compile(program)` call, so wrapping it in a salsa query later is ~one line of rework — the "avoid re-threading" risk this slice front-loads is cheap to satisfy just-in-time. To keep momentum on oracle-verified coverage (and avoid pulling in a heavyweight dependency for zero behavior change), this plumbing is **deferred to land immediately before the checker (M1.7)**, where it first earns its keep. The runtime feature slices (M1.2 functions, M1.3 collections, …) proceed first.

## Goal
Thread the compile pipeline through a salsa query graph **before** the checker needs it, so later slices edit a graph rather than rewrite a straight-line pipeline.

## Scope
- In:
  - **`lang-db`** crate: the salsa database with one input (`source_text`) and memoized pass-through queries `tokens(db)`, `ast(db)`, `chunk(db)` — each just calls the existing `lex`/`parse`/compile function and caches the result. Zero behavior change.
  - Route `VmBackend`'s compile path (and, where natural, the conformance runner) through `lang-db` queries.
- Out: any incremental-recompilation UX, the checker query (`checked_ast`, arrives M1.7), LSP/HMR consumers (M2).

## Checklist (vertical slice)
- [ ] Grammar / AST: none.
- [ ] Checker rule: n/a (this slice only stands up the graph the checker will plug into).
- [ ] Bytecode: `chunk(db)` query wraps `lang-compiler`.
- [ ] VM op: `VmBackend` consumes `chunk(db)` instead of calling the compiler directly.
- [ ] Conformance cases: existing corpus must remain differential-identical through the query layer (the oracle proves the wrap is behavior-preserving).
- [ ] Snapshots: none new; existing snapshots unchanged.

## Definition of done
- The full M1.0 subset still runs through `VmBackend`, now via `lang-db` queries, with `--differential` showing zero change in output or coverage %.
- `cargo test --workspace`, fmt, clippy clean.

## Notes / traps
- This is the main divergence from a naive "salsa only at the checker" reading: the *plumbing* lands now (cheap, behavior-preserving), the *checker* lands in M1.7. Doing it now avoids re-threading the entire pipeline through salsa during Thrust B.
- Keep queries as thin wrappers — no logic moves into `lang-db` yet; it only memoizes.

## Outcome (done)

Landed `lang-db` (salsa 0.27): one input `SourceProgram { id, name, text }` and three pass-through tracked queries forming the graph **`tokens(db) → ast(db) → bytecode(db)`**, each a thin wrapper over the existing `lang_lexer::lex` / `lang_parser::parse` / `lang_compiler::compile`. The checker query (`checked_ast`, M1.7) slots between `ast` and `bytecode` with no re-threading — the reason this plumbing front-loads.

**Foreign-result friction (the one real design point).** salsa memoizes a tracked function's output and needs it to be `Update` + `PartialEq`. The artifacts (`Lexed`/`Parsed`/`Module`) are foreign and implement neither, so each is wrapped in a local newtype (`Tokens`/`Ast`/`Bytecode`) given **conservative "always-changed"** impls: `PartialEq::eq` returns `false` (salsa never backdates) and a hand-written `unsafe impl Update` overwrites the slot in place and reports changed. Both are sound — salsa never serves a stale value; we only forgo backdating, which pass-through queries don't need. This is the crate's only `unsafe` (a 3-line always-replace), so `lang-db` opts out of the workspace `unsafe_code = "forbid"` like `lang-value`/`gc`/`vm`, and is miri-gated via a direct `maybe_update` test (the pass-through path never mutates an input, so salsa wouldn't otherwise exercise it).

**Wiring.** `VmBackend` gained `run_module(&Module)` (execute an already-compiled module), splitting compilation from execution so the VM "consumes `chunk(db)`" without `lang-vm` depending on `lang-db`. The conformance differential now drives the **whole** pipeline through the graph: both backends consume artifacts from the same `tokens`/`ast`/`bytecode` queries (tree-walker runs `ast(db).program`; VM runs `run_module(bytecode(db))`). The oracle proves the wrap is behavior-preserving — **33 matched / 0 skipped / zero divergence, unchanged from M1.6**. fmt/clippy clean; miri green on the unsafe.
