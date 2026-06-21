# Slice M1.1 — Salsa db plumbing

Status: todo

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
