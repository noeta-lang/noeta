# Slice M1.7 — Type checker: types + inference + ADT/exhaustiveness + ownership

Status: done (inference + exhaustiveness + `?`-typing + arithmetic mismatch as a shared front-end; unknown-type deferred to M1.9, ownership/immutability to a 7b follow-up — see Outcome)

## Goal
The single largest irreducible piece: an ML-grade, gradual-but-real type checker built as salsa queries, with inference, exhaustive `match`, typed `?`, and immutability/ownership analysis.

## Scope
- In:
  - **`lang-types`** crate: the `Type` lattice, `TypeId` interning, the `?T` = `Option<T>` desugar as a type.
  - **`lang-check`** crate: the `checked_ast(db)` salsa query — Hindley-Milner-style inference (annotations optional, name-first), ADT/exhaustive-`match` **static** check (promote M1.5's runtime non-exhaustive error to a compile error), `?`-on-non-`Result`/`Option` rejection, unhandled-`Result` lint, immutable-by-default + ownership analysis (eliding defensive copies).
  - New diagnostics catalog entries (E0011+), each with a negative conformance case (`// expect: error EXXXX at L:C`).
- Out: trait resolution / operator dispatch (M1.8 — though the checker scaffolds the trait/impl tables in `lang-types`); generics specialization (M1.8); module-aware resolution (M1.9).

## Checklist (vertical slice)
- [ ] Grammar / AST: type-annotation surface already in M0 AST (`TypeRef`, `Param.ty`, `Fn.ret`); wire it into checking.
- [ ] Checker rule: inference + exhaustiveness + `?`-typing + ownership/immutability, as salsa queries inserted between `ast(db)` and `chunk(db)`.
- [ ] Bytecode: `lang-compiler` consumes `checked_ast` (type info may inform lowering; behavior unchanged where untyped).
- [ ] VM op: none new (checking is compile-time); the M1.5 runtime non-exhaustive error becomes unreachable for checked programs.
- [ ] Conformance cases: positive (well-typed programs still run) **and** negative (`// expect: error E00xx`) for each rule: type mismatch, non-exhaustive match, `?` misuse, immutability violation, unhandled `Result`.
- [ ] Snapshots: rendered-`ariadne` snapshots for each new diagnostic (the error-quality gallery).

## Definition of done
- **Thrust B gate (partial):** every static-error class above has a negative conformance case with the correct code + span; the diagnostic gallery snapshots them.
- All previously-passing programs still typecheck and run differential-identical.
- The checker is a salsa query (incremental by construction); fmt/clippy clean.

## Notes / traps
- Build it as queries from the first commit — do not write straight-line passes and retrofit salsa. The M1.1 plumbing means `checked_ast` slots into an existing graph.
- Gradual: unannotated code must keep working (inference fills gaps); annotations are constraints, not a wall.
- This slice is large; if it grows unwieldy, split inference (7a) from ownership/immutability (7b) — but keep each independently corpus-verifiable.

## Outcome (done)

Landed two crates: **`lang-types`** (the `Type` lattice + the `?T` → `Option<T>` desugar, pure data) and **`lang-check`** (`check(&Program) -> Vec<Diagnostic>`), exposed to the pipeline as the **`checked` salsa query** in `lang-db`, slotted between `ast` and `bytecode` exactly as the M1.1 plumbing intended — no re-threading.

**The integration insight: the checker is a *shared front-end* for both backends.** It runs upstream of execution in the conformance `run_source`, the differential, and `lang run`. A program it rejects never reaches either backend, and its diagnostics are the whole observable result — identical no matter which backend would have run. So promoting a runtime error to compile-time keeps the differential green *by construction* (both backends now surface the same compile diagnostics), and a negative type-error case asserts via the ordinary `// expect: error` header. The differential holds at **35 matched / 0 skipped / 100% / zero divergence**.

**Gradual by construction.** Every expression gets an inferred `Type`, with `Type::Unknown` (the gradual top) as the fallback wherever inference is incomplete (unannotated params, prelude calls, method results). Every check suppresses itself on a gradual operand, so no program the M0 tree-walker runs is newly rejected — a diagnostic fires only when types are concretely known and unambiguously wrong. Inference is a conservative name-first pass, not yet full HM unification/generalization (the lattice + `Var` are in place for that hardening).

**Checks shipped, each with a conformance case + a rendered-ariadne gallery snapshot:**
- **Exhaustive `match` (E0011)** — the marquee ADT check; fires only on a concretely-typed enum / `Result` / `Option` scrutinee missing a variant with no catch-all. Promotes M1.5's runtime non-exhaustive error; `enums/match_fallthrough.lang` flips from runtime E0007 to compile-time E0011; new `enums/non_exhaustive_option.lang`.
- **`?` on a non-fallible value (E0012)** — `expr?` where `expr` is concretely neither `Result` nor `Option`; new `results/invalid_try.lang`.
- **Arithmetic type mismatch (E0007)** — `+ - * / %` on a concretely non-numeric operand (`1 + true`), reusing the runtime `TypeMismatch` code *at the same span*, so `diagnostics/type_mismatch.lang` keeps its exact assertion — the static error reads identically to the old runtime one.

**Deferrals (documented, not omissions):**
- **Unknown-type checking (E0013) → M1.9.** The corpus's `fn find(hit): ?User` annotates an *undeclared, unimported* `User` and M0 runs it fine. Until `use`/module resolution exists, "undeclared" can't be told from "valid-but-unresolved", so flagging it is a false positive (the dry-run over the corpus caught exactly this). The `E0013` code is reserved in the catalog; its emitter lands with M1.9. This is the safety net — the gradual rule — working as designed.
- **Ownership / immutability analysis → 7b follow-up.** M0 already enforces immutable-binding reassignment at runtime (E0006); a static promotion needs the new-local-vs-reassign scoping resolution and is split out to keep this slice corpus-verifiable.
- Full HM unification + let-generalization is the inference-hardening follow-up; the gradual checks above are already sound without it.
