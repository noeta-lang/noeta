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

- **Unknown trait** (`E0014`) — an `impl Trait { ... }` block or a `@derive(Trait)` directive names a trait that is not a built-in (or, for `@derive`, not a *derivable* one). Validated against the `BuiltinTrait` registry in `lang-types`.
- **Invalid impl** (`E0015`) — an `impl` block does not satisfy the trait it names: the trait's required method is missing or has the wrong arity (e.g. `impl Add` without an `add(other)` method).
- **Invalid attribute** (`E0017`) — a `#[...]` data attribute is malformed or misused — currently the migration case where `#[derive(...)]` (the old codegen spelling) appears in data-attribute position. Code generation is now the separate `@derive(...)` directive; `#[...]` is for data attributes only.

## What it checks (M1.9)

- **Unknown type** (`E0013`) — a type annotation (parameter, return, field, enum backing, or generic argument) naming a type that resolves to nothing: not a built-in, not one of the bare prelude spellings (`list`/`map`/`set`/`Ordering`), not a declared struct/class/enum, not a name brought in by a `use`, and not a generic parameter in scope. Deferred until now for a reason — before module resolution, "undeclared" could not be told apart from "valid but imported", risking a false positive on e.g. a `?User` return whose `User` came from a `use`. With the loader merging resolved imports into the program and leaving opaque-stub `use`s in place, both referents are visible to the checker's collect pass, so an unresolvable name is genuinely unknown.

These are declaration-level checks (they never touch expression typing), so they stay gradual-safe: a correct program emits nothing, and the differential oracle is unaffected. Code generation (`@derive(...)`) and data attributes (`#[...]`) are two distinct decorators with two distinct sigils: `@derive` names derivable traits the compiler synthesizes; `#[...]` attaches a struct as metadata (collected into the queryable build manifest; a `#[Foo(...)]` use is gated on `Foo` being a struct marked `@attribute`, with its arguments checked as a construction of `Foo` — E0029/E0009/E0007/E0005). The *behavior* the traits drive is wired in both backends (`lang-eval`, `lang-vm`): every operator is trait-dispatched — `+ - * / ~` (`Add`/…), `==`/`!=` (`Equatable`), and `< <= > >=` (`Comparable`, via the built-in `Ordering` enum + `.compare()`). The protocols dispatch too: `a[i]` via `Index` (`get`, over lists/maps/strings with built-in fallback, `E0016`/`E0018`), `len(o)` via `Length`, `echo`/interpolation via `Display` (`to_string`), and `for x in o` via `Iterable` (`iter`). Two derives synthesize genuinely-new behavior — `@derive(Comparable)` (structural field-wise ordering) and `@derive(ToJson)` (structural JSON) — while `Equatable`/`Display`/`Clone` are the checked spelling of M1's structural defaults. Generics are erased: `class Box<T>` parses, checks gradually (a type parameter is `Unknown`), and runs with `T` erased at runtime. M1.8 is complete, and the attribute-system pass (`plans/attributes/`) added the standalone `impl Trait for T {}` capability mechanism and the `@attribute`-directive gate on `#[...]` usage; the remaining tail (`Callable`/`Members` protocols, monomorphic shape specialization) is tracked in `plans/m1/slice-08-traits.md`.

Inference is **bidirectional checking with local inference**, deliberately *not* Hindley–Milner — subtyping (`dyn` widening, directional method resolution, struct width) is load-bearing and defeats HM's symmetric unification. The inferred-static type-system track (`plans/types/`) built this out: required signatures at named boundaries, checked arguments/returns, the `E0023` binding endpoint, bounded generics, trait coherence, `dyn` narrowing, and declared unions — with only an *interior* inference hole tolerated by design. Immutability/ownership analysis is the 7b follow-up.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
