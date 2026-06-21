# Slice M1.8 — Traits as operators + built-in derives + generics

Status: todo

## Goal
Unify operators and built-in protocols under one trait mechanism, add compiler-implemented derives, and make generics fall out of shapes.

## Scope
- In:
  - The trait system as the single operator/protocol mechanism: `Add`/`Sub`/`Mul`/`Div`/`Concat` (`+ - * / ~`), `Equatable` (`==`/`!=`), `Comparable` (`< <= > >=` from `compare`), `Index` (`a[i]`), `Display` (interpolation/`echo`), `Length` (`len`/`.count()`), `Iterable` (`for`), `Callable` (`a(...)`); default methods compose (implement `compare` ⇒ all four comparisons). Fallible variants `TryAdd`/`TryComparable` returning `Result`.
  - Operator desugaring routes through trait resolution in `lang-check`; lowering in `lang-compiler` dispatches to the resolved impl.
  - **Built-in derives implemented in the compiler** (`ToJson`, `Equatable`, `Comparable`, `Clone`) — users *apply* `#[derive(...)]`, cannot *write* derives (no comptime/macros).
  - **Generics** via shapes (`Collection<User>` = shape with a type-param slot; the check is an inline-cache guard, elided when monomorphic).
  - The **attribute manifest**: attributes are ordinary records in annotation position; the compiler keeps the index as a build artifact.
- Out: runtime reflection / `type_of` (M2); user-defined derives (deferred indefinitely); WASM-sandboxed lint rules (M2).

## Checklist (vertical slice)
- [ ] Grammar / AST: `trait`/`impl`/`#[derive(...)]`/attribute surface (extend AST as needed); generic type parameters on decls.
- [ ] Checker rule: trait resolution, impl coherence, derive expansion (in-compiler), generic instantiation checking, attribute-`Attribute`-marker validation.
- [ ] Bytecode: operator opcodes dispatch through resolved trait impls; derive-generated methods compiled.
- [ ] VM op: trait-method call-site caches (monomorphic fast path); generic guard elision (`lang-vm` + `lang-object`).
- [ ] Conformance cases: operator-via-trait, custom `Add`/`Comparable` impls, each built-in derive, a generic container, fallible `TryAdd` through `?`, an attribute read from the manifest.
- [ ] Snapshots: disassembly showing operator → trait dispatch; rendered diagnostics for coherence/derive errors.

## Definition of done
- **Thrust B gate (complete):** operators are trait-dispatched end-to-end; the four built-in derives work; a generic type checks and runs; the attribute manifest is queryable.
- All prior corpus cases still pass (operators on built-in types now route through the trait system without behavior change).
- miri green; fmt/clippy clean.

## Notes / traps
- `~` and `+` on built-in types must produce identical observable behavior to M0 now that they route through `Concat`/`Add` — the oracle guards this.
- Destruction stays a distinct construct (`destruct`), **not** a trait — preserve the M1.6 boundary.
- Generics are erasure-for-storage / reification-for-identity via shapes — not template monomorphization. Flat-array specialization for packed types is M2 (design the type-system distinction here only).
