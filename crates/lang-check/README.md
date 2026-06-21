# lang-check

The type checker: the gradual, static front-end between parsing and compilation.

- **Takes in:** `Program` (from `lang-ast`), reasoning in `Type` (from `lang-types`).
- **Emits:** `check(&Program) -> Vec<Diagnostic>` — the type errors found (empty ⇒ well-typed). This is the body of the `checked` salsa query in `lang-db`, slotted between `ast` and `bytecode`.

## A shared front-end

The checker runs upstream of *both* backends (in the conformance harness, the differential, and `lang run`). A program it rejects never reaches the tree-walker or the VM, and its diagnostics are the program's whole observable result — identical regardless of which backend would have run. That is what lets M1.7 promote runtime errors to compile-time ones while keeping the differential oracle green by construction, and lets a negative type-error case assert via the ordinary `// expect: error EXXXX` header.

## Gradual by construction

Every expression gets an inferred `Type`, with `Type::Unknown` (the gradual top) as the fallback wherever inference is incomplete (unannotated parameters, prelude calls, method results). **Every check suppresses itself on a gradual operand**, so no program the M0 tree-walker runs is newly rejected — a diagnostic fires only when types are *concretely* known and unambiguously wrong.

## What it checks (M1.7)

- **Exhaustive `match`** (`E0011`) — a concretely-typed enum / `Result` / `Option` scrutinee missing a variant with no catch-all (promotes M1.5's runtime non-exhaustive error).
- **`?` on a non-fallible value** (`E0012`) — `expr?` where `expr` is statically neither `Result` nor `Option`.
- **Arithmetic type mismatch** (`E0007`) — `+ - * / %` on a concretely non-numeric operand, reusing the runtime `TypeMismatch` code at the same span.

## What it checks (M1.8a)

- **Unknown trait** (`E0014`) — an `impl Trait { ... }` block or a `#[derive(Trait)]` attribute names a trait that is not a built-in (or, for `derive`, not a *derivable* one). Validated against the `BuiltinTrait` registry in `lang-types`.
- **Invalid impl** (`E0015`) — an `impl` block does not satisfy the trait it names: the trait's required method is missing or has the wrong arity (e.g. `impl Add` without an `add(other)` method).

These are declaration-level checks (they never touch expression typing), so they stay gradual-safe: a correct program emits nothing, and the differential oracle is unaffected. The *behavior* the traits drive is wired in both backends (`lang-eval`, `lang-vm`): the infix operators `+ - * / ~` (M1.8a) and `==`/`!=` via `Equatable` (M1.8b). `Comparable` ordering (needs an `Ordering` type), derive codegen, the remaining protocols, and generics are the rest of M1.8b.

Inference is a conservative name-first gradual pass, not yet full Hindley–Milner unification/generalization (the lattice and `Type::Var` are in place for that hardening). Unknown-type checking (`E0013`) is deferred to M1.9 (it needs `use`/module resolution to tell "undeclared" from "valid-but-unresolved"); immutability/ownership analysis is the 7b follow-up.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
