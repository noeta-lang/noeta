# Slice S4.3 — Complete generic enforcement (the items S4.2 deferred)

Status: **in-progress** (S4.3a → S4.3b → S4.3c)

> **Track:** inferred-static type system (see `plans/types/README.md`). **Follows:** S4.2. **Determinism / oracle posture:** still front-end only — the runtime stays erased, so every accepted program runs identically on both backends and `--differential` stays at **0 skipped**. The effect is *more* compile-time rejections (the `E0025` surface widens) and *more precise* typing of associated calls.

## Why

S4.2 enforced bounds on generic *function* calls but explicitly deferred three things. The user asked for them next; this slice closes all three.

1. **Associated/static calls type as a hole.** `Box.new(1)` is `Expr::Member { receiver: Ident("Box"), name: "new" }`, and a bare type name in receiver position is not resolved as a value, so `synth(Ident "Box")` is `Unknown` and the whole call defers. Consequently a generic call whose arguments come from constructors never binds a type parameter — no enforcement, result `dyn`.
2. **Generic-class bounds are recorded but not enforced.** `class Sorted<T: Comparable>` validates its bound (S4.1) but instantiating `T` at construction/method-call is unchecked.
3. **Body-side bound requirement is absent.** Using a trait operation on an *unbounded* `T` inside a generic body (`fn f<T>(a: T, b: T) { a < b }`) is not rejected at the definition.

## Sub-slices (each its own green commit)

### S4.3a — Associated / static call typing

In `synth_call`'s `Member` arm, before the instance-method path: if the receiver is a bare identifier naming a **known user type** (and not shadowed by a local), resolve `Type.method(args)` to the type's registered method signature, check the arguments, and return its declared return type. `Box.new(1)` now types as `Box`, not a hole. A new `call_user_method` helper unifies this with the instance-method path (both resolve a `methods[(Type, name)]` signature; generic ones route through `check_generic_call`). This *also* makes constructor results flow into generic-*function* bound enforcement: `max(Box.new(1), Box.new(2))` now binds `T = Box` and so reports `E0025` (where before it ran and failed at runtime).

### S4.3b — Generic-class construction/method bound enforcement

Populate `FnSig.generic` for the methods of a **generic class** — the class's type parameters with their bounds, plus the method's *un-erased* parameter/return types — so a call routes through the existing `check_generic_call`. A construction `Sorted.new(x)` then binds the class's `T` from the constructor's argument and enforces `T: Comparable` (`E0025`). Instance-method calls route the same way; where `T` appears only in the receiver (not the method's parameters) it stays unbound and unenforced — the limitation inherent to erasing the receiver's type arguments (`Type::Named` carries no arguments), recorded rather than papered over.

### S4.3c — Body-side bound requirement

An ordering comparison (`< <= > >=`) whose operand is an in-scope type parameter requires that parameter to carry the `Comparable` bound; otherwise `E0025` at the definition (Rust-style: the generic body is rejected before any call). This needs the in-scope bounds, so `Checker::type_params` becomes a `name → bounds` map. Scope is deliberately narrow — only ordering-on-a-type-parameter — since general operator-as-trait checking on concrete types (`obj < obj`, today a runtime error) is a separate, pre-existing gap, not one this slice owns.

## Deferred still (honest boundary)

- **Tracking a type argument through an instance** (`Box<string>` as a value type) would require `Type::Named` to carry arguments — a lattice-wide change, out of scope. So a bound that only constrains a method's *receiver-shaped* use is not enforced at instance-method calls.
- **Operator/bound correspondence beyond ordering→`Comparable`** (`==`→`Equatable`, `+`→`Add`, …) is not enforced body-side; ordering is the canonical case and the others ride the same machinery when wanted.

## Oracle posture

Front-end only, checker shared ⇒ differential holds, **0 skipped**. Baseline at S4.3 start: conformance 138 / differential 132 matched / 0 skipped.

## Verification (per sub-slice)

- `cargo run -q -p lang-cli -- test --differential` → matched / **0 skipped** / backends agree.
- `cargo run -q -p lang-cli -- test` → full conformance green.
- `cargo test -p lang-check` + clippy + fmt clean.
