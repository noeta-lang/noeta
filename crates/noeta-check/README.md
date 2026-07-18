# noeta-check

The type checker: the inferred-static front-end between parsing and compilation.

- **Takes in:** `Program` (from `noeta-ast`), reasoning in `Type` (from `noeta-types`).
- **Emits:** `check(&Program) -> Vec<Diagnostic>` — the type errors found (empty ⇒ well-typed). This is the body of the `checked` salsa query in `noeta-db`, slotted between `ast` and `bytecode`.

## A shared front-end

The checker runs upstream of *both* backends (in the conformance harness, the differential, and `noeta run`). A program it rejects never reaches the tree-walker or the VM, and its diagnostics are the program's whole observable result — identical regardless of which backend would have run. That is what lets the checker promote runtime errors to compile-time ones while keeping the differential oracle green by construction, and lets a negative type-error case assert via the ordinary `// expect: error EXXXX` header.

## Inferred-static, with an interior inference hole

Every expression gets an inferred `Type`. Signatures are **required at named boundaries** (a missing parameter or return type is `E0022`) and inference runs locally in bodies, with `Type::Unknown` as the fallback wherever a type is genuinely not yet known (e.g. an erased generic parameter). A check suppresses itself on an `Unknown` operand, so the *interior* tolerates an inference hole by design — but holes are eliminated at typed boundaries, so a program with an un-inferable binding (`E0023`) or a missing signature (`E0022`) is rejected, and `dyn` is the one explicit dynamic escape. (This is a change from the earlier *gradual* posture, in which every check suppressed on incomplete information; see the inferred-static track below.)

## What it checks (M1.7)

- **Exhaustive `match`** (`E0011`) — a concretely-typed enum / `Result` / `Option` scrutinee missing a variant with no catch-all (promotes M1.5's runtime non-exhaustive error).
- **`?` on a non-fallible value** (`E0012`) — `expr?` where `expr` is statically neither `Result` nor `Option`.
- **Arithmetic type mismatch** (`E0007`) — `+ - * / %` on a concretely non-numeric operand, reusing the runtime `TypeMismatch` code at the same span.

## What it checks (M1.8a)

- **Unknown trait** (`E0014`) — an `impl Trait { ... }` block or a `@derive(Trait)` directive names a trait that is not a built-in (or, for `@derive`, not a *derivable* one). Validated against the `BuiltinTrait` registry in `noeta-types`.
- **Invalid impl** (`E0015`) — an `impl` block does not satisfy the trait it names: the trait's required method is missing or has the wrong arity (e.g. `impl Add` without an `add(other)` method).
- **Invalid attribute** (`E0017`) — a `#[...]` data attribute is malformed or misused — currently the migration case where `#[derive(...)]` (the old codegen spelling) appears in data-attribute position. Code generation is now the separate `@derive(...)` directive; `#[...]` is for data attributes only.

## What it checks (M1.9)

- **Unknown type** (`E0013`) — a type annotation (parameter, return, field, enum backing, or generic argument) naming a type that resolves to nothing: not a built-in, not one of the bare prelude spellings (`list`/`map`/`set`/`Ordering`), not a declared struct/class/enum, not a name brought in by a `use`, and not a generic parameter in scope. Deferred until now for a reason — before module resolution, "undeclared" could not be told apart from "valid but imported", risking a false positive on e.g. a `?User` return whose `User` came from a `use`. With the loader merging resolved imports into the program and leaving opaque-stub `use`s in place, both referents are visible to the checker's collect pass, so an unresolvable name is genuinely unknown.

These are declaration-level checks (they never touch expression typing), so a correct program emits nothing and the differential oracle is unaffected. Code generation (`@derive(...)`) and data attributes (`#[...]`) are two distinct decorators with two distinct sigils: `@derive` names derivable traits the compiler synthesizes; `#[...]` attaches a struct as metadata (collected into the queryable build manifest; a `#[Foo(...)]` use is gated on `Foo` being a struct marked `@attribute`, with its arguments checked as a construction of `Foo` — E0029/E0009/E0007/E0005). The *behavior* the traits drive is wired in both backends (`noeta-eval`, `noeta-vm`): every operator is trait-dispatched — `+ - * / ~` (`Add`/…), `==`/`!=` (`Equatable`), and `< <= > >=` (`Comparable`, via the built-in `Ordering` enum + `.compare()`). The protocols dispatch too: `a[i]` via `Index` (`get`, over lists/maps/strings with built-in fallback, `E0016`/`E0018`), `len(o)` via `Length`, `echo`/interpolation via `Display` (`to_string`), and `for x in o` via `Iterable` (`iter`). Two derives synthesize genuinely-new behavior — `@derive(Comparable)` (structural field-wise ordering) and `@derive(ToJson)` (structural JSON) — while `Equatable`/`Display`/`Clone` are the checked spelling of M1's structural defaults. Generics are erased: `class Box<T>` parses, checks with the type parameter treated as `Unknown`, and runs with `T` erased at runtime. M1.8 is complete, and the attribute-system pass (`plans/attributes/`) added the standalone `impl Trait for T {}` capability mechanism and the `@attribute`-directive gate on `#[...]` usage; the remaining tail (`Callable`/`Members` protocols, monomorphic shape specialization) is tracked in `plans/backlog.md`.

Inference is **bidirectional checking with local inference**, deliberately *not* Hindley–Milner — subtyping (`dyn` widening, directional method resolution, struct width) is load-bearing and defeats HM's symmetric unification. The inferred-static type-system track (arc ledger in `plans/` git history) built this out: required signatures at named boundaries, checked arguments/returns, the `E0023` binding endpoint, bounded generics, trait coherence, `dyn` narrowing, and declared unions — with only an *interior* inference hole tolerated by design. Immutability/ownership analysis is the 7b follow-up.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
