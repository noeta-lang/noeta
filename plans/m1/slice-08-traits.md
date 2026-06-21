# Slice M1.8 — Traits as operators + built-in derives + generics

Status: in progress (M1.8a done; M1.8b todo)

Split, following the M1.6a/6b precedent: **M1.8a** lands the trait/operator/attribute *surface* and wires infix operator overloading end-to-end through both backends (the headline, differential-guarded feature). **M1.8b** lands the behavior behind the remaining protocols (derive codegen, structural `Comparable`, `Display`/`Index`/`Members`/`Callable` dispatch, fallible `Try*`), generics, and the attribute manifest.

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

## M1.8a — done (this slice)
The surface, the registry, the checker rules, and infix operator overloading wired through both backends.

- [x] Grammar / AST: `impl Trait { ... }` blocks inside class bodies (`lang-lexer` `impl`/`#` tokens, `lang-ast` `ImplBlock`); `#[Name(args)]` attributes on class/record/enum (`Attribute`); `BinaryOp::overload_method` (the op → method name map both backends share). Impl-block methods are flattened into `ClassDecl::methods`, so the existing `(type, method)` dispatch resolves them with no change.
- [x] Checker rule: the built-in trait registry (`lang-types::BuiltinTrait` + `BUILTIN_TRAITS`, the single source of truth, lock-stepped to `overload_method` by a unit test). `lang-check` validates `impl` trait names (**E0014** unknown trait), the required method's presence + arity (**E0015** invalid impl), and `#[derive(...)]` names (**E0014**, derivable subset). Decl-level only — gradual-safe, no expression-typing change.
- [x] VM op + tree-walker: operator dispatch for `+ - * / ~`. When the left operand is a user object whose class defines the operator's trait method, the operation routes to that method (`eval_binary`; VM `Op::Binary` pushes a frame exactly like a method call); otherwise the built-in operator semantics apply unchanged. Left-biased, matching the syntax doc.
- [x] Conformance cases: `traits/operator_add.lang` (custom `Add`, runs identically in both backends), `traits/derive_value_object.lang` (`#[derive(...)]` validated + structural `==`), `traits/unknown_trait.lang` (E0014), `traits/invalid_impl.lang` (E0015).
- [x] Snapshots: checker diagnostic gallery extended with E0014/E0015; VM/checker unit tests; `lang-types` consistency test.

## M1.8b — in progress

**Done (increment 1 — equality + fallible):**
- [x] **`Equatable` dispatch** — `impl Equatable { fn eq(other): bool }` lights up `==`/`!=` in both backends, overriding the default structural equality. `!=` negates `eq`'s result via a `RetTransform` on the VM frame (the call-then-transform mechanism the infix group didn't need); the tree-walker negates synchronously. Conformance `traits/operator_eq.lang`.
- [x] **Fallible `TryAdd`** — needs *no* operator wiring: bare `+` is reserved for infallible `Add`, and a fallible add is the explicit `try_add` returning `Result`, composed with `?` (an ordinary method call + `?`, both already supported). The `impl TryAdd` is validated by the 8a checker (E0015 if `try_add` is missing). Conformance `traits/fallible_try_add.lang`.

**Done (increment 2 — ordering):**
- [x] **`Ordering` built-in enum** (`Less`/`Equal`/`Greater`) + **`.compare()` on primitives** (int/float/string), both backends. `Ordering` values are constructed on the fly (eval `builtin_enum`; VM an on-the-fly `Shape::enum_variant`, like `MakeOpaque`) — shapes carry no identity (match/equality are by name + variant), so the two backends' values are interchangeable, keeping the differential identical. The values are ordinary enums: they display (`Ordering.Less`) and **`match` by variant** (`traits/match_ordering.lang`). Conformance `traits/compare_primitives.lang`.
- [x] **`Comparable` dispatch** — `impl Comparable { fn compare(other): Ordering }` lights up `< <= > >=` in both backends; the returned `Ordering` is mapped to each operator's bool (`<` ⇒ `Less`, `<=` ⇒ `Less`/`Equal`, …) via the new `RetTransform::Ordering(op)` variant (VM) / synchronously (eval). The canonical body delegates to `.compare()` on a field, exactly as the syntax doc shows. Conformance `traits/comparable.lang`. (A refcount-leak trap: the VM's `Ordering` → `bool` transform discards a *heap* value, so the frame's keep-alive reference must be released — `apply` now reports whether it replaced the value; miri-gated.)

**Done (increment 3 — derive codegen, `Comparable`):**
- [x] **`#[derive(Comparable)]` → structural ordering** — a value object (class or record) deriving `Comparable` without a hand-written `compare` gets field-wise `< <= > >=`, comparing fields in **declared order** (lexicographic). This is the one derive that adds genuinely-new, must-be-gated behavior: `==`/display/copy are already M1's structural defaults (so `#[derive(Equatable/Display/Clone)]` is the explicit, checked spelling of the default), but `<` errored on objects before. The compiler records the deriving type names into the `Module` (`comparable_derives`) / eval's `TypeDef` (`derives_comparable`); the dispatch computes the order synchronously via `lang_value::structural_compare` (VM) / `object_structural_compare` (eval) — both lexicographic over slots in declared order, so the backends agree. A hand-written `impl Comparable` takes precedence (the type is excluded from the derived set). Conformance `traits/derive_value_object.lang` (class, the flagship example) + `traits/derive_comparable_record.lang` (record). Fields must be primitive (nested-object fields are incomparable for now).

**Todo:**
- [ ] User-facing `Ordering.Less` construction (the dispatch needs only delegation via `.compare()`, so this is deferred); register `Ordering` as a namable prelude enum.
- [ ] The remaining derive *codegen*: `Display` (`to_string`), `ToJson` — genuinely new (`ToJson` needs a JSON serializer; `Equatable`/`Display`/`Clone` already match M1's structural defaults). Nested-object fields in derived `Comparable` (recurse into sub-objects).
- [ ] Other protocols: `Index` (`a[i]`), `Length` (`len`), `Iterable` (`for`), `Callable` (`a(...)`), `Members`/`DynamicCall` — each needs the surface operator routed to its method.
- [ ] Generics via shapes (type-param slot, monomorphic guard elision) + the attribute manifest build artifact.
- [ ] Inline-cache fast path for trait-method call sites (perf; currently a per-op hashmap lookup on the object path).

## Definition of done
- **M1.8a gate (met):** operator overloading is trait-dispatched end-to-end in both backends, differential-identical; every trait/derive misuse has a negative conformance case; the surface parses/checks as the syntax doc shows.
- **Thrust B gate (M1.8b):** the four built-in derives work; a generic type checks and runs; the attribute manifest is queryable.
- All prior corpus cases still pass (operators on built-in types are untouched). miri green on the VM path; fmt/clippy clean.

## Notes / traps
- `~` and `+` on built-in types must produce identical observable behavior to M0 — the overload only engages when the **left operand is a user object** with the trait method, so built-ins are never rerouted. The oracle guards this.
- Destruction stays a distinct construct (`destruct`), **not** a trait — preserve the M1.6 boundary.
- Generics are erasure-for-storage / reification-for-identity via shapes — not template monomorphization. Flat-array specialization for packed types is M2 (design the type-system distinction here only).
- The infix operator group (`Add`/`Sub`/`Mul`/`Div`/`Concat`) was chosen for 8a because each returns its result value directly — a frame-based VM can dispatch it as a plain method call. `Equatable`/`Comparable` need post-call processing (negate for `!=`, ordering→bool for `<`), which is why their wiring is 8b.

## Outcome (M1.8a)
Landed: `impl`/`#` tokens; `ImplBlock`/`Attribute` AST + `BinaryOp::overload_method`; parser for attributes + impl blocks (methods flattened into the class table); `lang-types::BuiltinTrait` registry (lock-stepped to `overload_method`); `lang-check` impl/derive validation (E0014/E0015); operator dispatch in `eval_binary` and VM `Op::Binary`. Differential held at **39 matched / 0 skipped / 100% / zero divergence** (+4 trait cases; `operator_add` runs identically in both backends). miri green on the new VM path. The behavioral tail (derive codegen, non-operator protocols, fallible operators, generics, manifest) is M1.8b above.
