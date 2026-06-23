# Slice S4 — Explicit bounded generics, statically enforced (E0025)

Status: **done** (S4.1 + S4.2 landed)

> **Track:** inferred-static type system (see `plans/types/README.md`). **Depends on:** S3c.4 (the inference endpoint) + the bidirectional engine (S1) + argument checking (S3b). **Determinism / oracle posture:** bounds are a *static* layer — checked at compile time, **erased at runtime** exactly like the type parameters they constrain. The runtime representation and dispatch are untouched (generics stay fully erased), so both backends run every accepted program identically and `--differential` stays at **0 skipped**. The only observable effect is that *more programs are rejected at compile time* (a new `E0025`), and generic-function calls now infer a *precise* return type instead of leaking `dyn`.

## Goal

Make `<T: Comparable>` real: a declaration may constrain each type parameter with one or more built-in trait bounds, and the checker **enforces** those bounds where the generic is instantiated. This is the locked S4 decision — explicit, Rust-style bounds at the declaration site, statically enforced — and it is the first slice that gives type parameters *meaning* beyond a name (today they are erased strings the body may use freely).

The canonical program:

```
fn max<T: Comparable>(a: T, b: T): T {
    if a > b { return a; }
    return b;
}

max(3, 5);          // ok — int satisfies Comparable, result type is int
max("a", "b");      // ok — string satisfies Comparable
```

and the rejection:

```
class Box<T> { value: T; }
max(Box.new(1), Box.new(2));   // E0025 — Box does not satisfy Comparable
```

## Why this is safe

Generics are **erased** — `class Box<T>` and (new this slice) `fn max<T>` compile to dispatch over `dyn`, no monomorphization. So bounds add a *rejecting* static layer on top of an unchanged runtime: a program the checker accepts runs exactly as it did before bounds existed, on both backends. The bound check is the only new front-end behavior; there is **no new VM surface** (the one runtime op in the whole track is S6's `as<T>()` narrowing), so the differential holds by construction.

## Sub-slices (each its own green commit)

### S4.1 — Declaration syntax + bound validation (additive surface) — **done**

Purely additive: the grammar grows bounds, functions become generic, and bound *names* are validated. No call-site semantics change yet, so the corpus is green by construction (no corpus program is generic-over-a-bound today).

**AST.** A new `TypeParam { name: String, bounds: Vec<String>, span: Span }` replaces the bare `Vec<String>` on `RecordDecl` / `ClassDecl` / `EnumDecl`, and is **added** to `FnDecl` (`type_params: Vec<TypeParam>`) — functions can now be generic for the first time. `bounds` is empty for an unbounded `<T>`.

**Parser.** The shared `type_params` combinator parses `<T>`, `<A, B>`, `<T: Comparable>`, and multi-bound `<T: Comparable + Display>` (Rust-style `+`). `fn_decl` now threads `type_params` between the name and the parameter list. In declaration position a `<` immediately after the declared name is unambiguous (no comparison expression can appear there).

**Checker.** Each bound must name a known built-in trait (`BuiltinTrait::lookup`) — an unknown bound is `E0014 UnknownTrait`, reusing the existing `impl`/`@derive` validation path. Function type parameters are brought into scope for the body exactly as class/record/enum parameters are (the `type_params` `HashSet<String>` of names, erased to `dyn` in signatures), so a generic function body type-checks with `T` a known type.

**Pretty.** `type_params_str` renders bounds (`<T: Comparable>`); unbounded params render `<T>` unchanged, so existing generic snapshots are byte-identical.

### S4.2 — Instantiation + bound enforcement (E0025) — **done**

The semantics. At a **generic-function call site** the checker performs local instantiation (no global solver — the inferred-static engine's local inference):

1. Bind each type parameter by structurally matching the declared parameter types (with `T` un-erased) against the argument types: `max(3, 5)` matches `T` against `int` twice → `{T: int}`; `f(xs: List<T>)` matches `List<T>` against `List<int>` → `{T: int}`. A parameter that pins `T` to two different concrete types takes their join (or is an argument `E0007` against the first binding).
2. Check each argument against the **substituted** parameter type, so `max(3, "x")` is `E0007` (second arg `string` against the already-bound `int`).
3. For each type parameter carrying bounds, check the binding **satisfies** every bound; a violation is **`E0025 TraitBoundNotSatisfied`** ("type `Box` does not satisfy bound `Comparable` on type parameter `T`").
4. The call's result type is the **substituted** return type — `max(3, 5): int`, not `dyn`. This is why instantiation (not erasure-to-`dyn`) is the right design: it keeps generic results precisely typed, the whole point of inferred-static.

**Satisfaction model** (`satisfies(ty, trait_name) -> bool`), new and the foundation S5 builds on:
- **Built-in types**: a fixed table — `int`/`float`/`string`/`bool` satisfy `Comparable`, `Equatable`, `Display`; numeric types satisfy `Add`/`Sub`/`Mul`/`Div`; etc. (mirrors what the backends actually dispatch).
- **User types**: a `(type_name -> {trait})` index built in the pre-pass from each declaration's `@derive(...)` list and `impl Trait` blocks (the data `check_derives` / `check_impl` already visit — this slice *records* it instead of only validating names).
- `dyn` and inference holes satisfy every bound (deferred to runtime / no information — never a false positive).

**Diagnostic.** `DiagnosticCode::TraitBoundNotSatisfied -> E0025` (append-only: enum, `ALL`, `code()`).

**Conformance.** `generics/bounded_ok.lang` (the `max` program, runs on both backends), `generics/bound_violated.lang` (`E0025`), `generics/bound_arg_mismatch.lang` (`E0007` from the substituted second arg), `generics/unknown_bound.lang` (`E0014`). Checker unit tests for the binding/satisfaction matrix.

## Outcome (S4.2)

Landed as designed: `synth_call`'s function-call arm branches on a new `FnSig.generic` (`GenericInfo`: the type parameters with bounds, plus the un-erased parameter/return types). `check_generic_call` binds each parameter left-to-right from the argument types (`bind_type_params`, a deferred argument never pins a parameter so a later concrete one can), checks each argument against its substituted parameter (`E0007`), enforces bounds via `satisfies` (`E0025`), and returns the substituted result (residual parameters erased to `dyn`). `satisfies` consults a `(type → traits)` index built from `@derive`/`impl` (new `trait_impls` map) for user types and a fixed `builtin_satisfies` table for built-ins. Conformance **138 / differential 132 matched / 0 skipped / backends agree**; 5 new checker unit tests, 4 new conformance cases.

**Finding — a static-method call (`Box.new(1)`) currently types as a hole**, because a bare type name in receiver position is not resolved as a value, so `recv` is `Unknown` and the call defers. Consequently a generic call whose arguments come from constructors is *not* bound (its parameters stay unconstrained → no `E0025`, result `dyn`), and the program would only fail at runtime. The conformance/unit cases therefore exercise `E0025` through **record/object literals** (which do type concretely) rather than constructors. Typing associated/static calls precisely is a separate front-end gap (independent of bounds) — a candidate follow-up that would *widen* where enforcement bites; recorded here, not silently absorbed.

## Deferred within S4 (noted, not silently dropped)

- **Generic-class construction/method bound enforcement.** Bounds declared on a `class Foo<T: Comparable>` are *recorded and validated* (S4.1) but enforcement at construction/method-call instantiation is deferred — class generics are fully erased through method dispatch today, and threading instantiation through `obj.method(...)` is a larger change than the function-call case. Trigger: a corpus program that constructs a bounded generic class with a non-satisfying argument. Likely folded into S5 (coherence) or a dedicated follow-up.
- **Body-side bound *requirement*.** Using a trait operation on an *unbounded* `T` inside a generic body (`fn f<T>(a: T, b: T) { a < b }`) is not rejected. The runtime is erased and `.compare()` is universal, so this is not load-bearing for soundness of accepted programs (the call-site check guarantees any concrete `T` actually supports the bound operation). Rust rejects it for monomorphization's sake, which we do not have. Revisit if/when static dispatch (perf track) needs it.

## Oracle posture

Checker shared by both backends ⇒ every new rejection is identical on both and the differential holds by construction. S4.1 adds no rejection the corpus hits; S4.2 adds `E0025` only for genuinely unsatisfiable instantiations (none in the current corpus) and *improves* generic-call return typing. No new VM surface ⇒ `--differential` stays **0 skipped**. Baseline at S4 start: conformance 133 / differential 127 matched / 0 skipped.

## Verification (per sub-slice, before each commit)

- `cargo run -q -p lang-cli -- test --differential` → matched / **0 skipped** / backends agree.
- `cargo run -q -p lang-cli -- test` → full conformance green (count grows with new cases).
- `cargo test -p lang-check -p lang-types -p lang-parser -p lang-ast` (new bound-parsing, validation, instantiation, satisfaction, and `E0025` unit tests).
- `cargo clippy --all-targets` + `cargo fmt --all --check`.
