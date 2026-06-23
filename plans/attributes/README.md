# Attribute system — Pass 1 (`impl Trait for T` · attribute parameters · the `#[…]` gate)

Status: **pass 1 complete** (commits A1 `3cb663e` · A2 `9ea9e2d` · A3 `a70380e`). Conformance 182 / differential 176 matched / 0 skipped / backends agree. Branch `types-inferred-static`. Pass 2 (runtime read-back) not started.

The attribute machinery is largely built already — `#[…]` parses, type-checks loosely, and lands in the compiler manifest; `@derive(…)` codegen for `Comparable`/`ToJson` runs in both backends. What is missing is the part the three deferred items (`plans/deferred.md`) folded into a deliberate "attribute system" pass: a record/class can't yet *declare* it is usable as an attribute, attribute arguments are identifier-only, and nothing validates a `#[Foo(…)]` use against the record it names.

This is **pass 1 of two**. Pass 1 makes attributes *typed records/classes, validated at the use site, correctly captured in the manifest*. Pass 2 (later) is the read-back: runtime reflection (`attributes_of`/`type_of`), `AttachableTo` target constraints, and capability-gated reflection metadata + tree-shaking roots.

## The model (settled in discussion)

The record/class axis is **value semantics vs reference semantics**, not data-vs-behavior and not traits-vs-no-traits — a record already participates in traits (you `@derive(Comparable)` on one and it stays a value type). A record is a bodiless named map (`type Route = { path: string }`), so there is nowhere *inside* it to mark a capability. The capability is therefore declared from the outside, with a **standalone `impl Trait for T {}`** — which works uniformly for records and classes, rather than making records second-class. The orphan rule S5 guaranteed structurally is preserved by a **same-module guard**: you may only `impl` a capability for a type declared in your own module.

An attribute is then an ordinary record or class marked `impl Attribute for T {}`; `#[Foo("/x")]` constructs `Foo { path: "/x" }` through the same all-fields-literal machinery as any other value, checked at the use site.

## Slices (each a green commit)

| Slice | What | Mechanism / verification |
|---|---|---|
| **A1** | Standalone `impl Trait for T {}` — the capability mechanism (generalizes beyond attributes) | New `Stmt::Impl(ImplDecl)`; top-level parser rule (`for Type` distinguishes it from the class-body `impl Trait {}`); checker validates the trait via the existing `check_impl`, guards the target is locally declared (orphan → **E0013**), registers satisfaction onto the target's `type_traits`, and folds standalone impls into per-type `check_coherence` (**E0027**). Pass-1 scope: marker/capability traits (empty body), so **no backend change** — differential 0-skipped by construction. Oracle-visible (rejected programs are shared verdicts). |
| **A2** | Richer attribute arguments (attribute *parameters*) | Widen `#[Foo(…)]` from identifier-only to **literals** (string/int/float/bool/ident), positional + named. `Attribute.args` → `Vec<AttrArg>`; parser, `pretty.rs`, `AttributeRecord` (`lang-bytecode`), `record_attributes` (compiler). Literals only — no arbitrary expressions. Verified by parser snapshots + a manifest unit test (manifest content isn't in `RunResult`, so it is unit-tested, not differential — the intrinsic reason this was deferred). |
| **A3** | The `#[Foo(…)]` capability gate + construction check (**E0029**) | Checker: a `#[Foo(…)]` use requires `Foo` satisfies `Attribute` (true via A1) else **E0029**; the args must construct a valid `Foo` — reuse record-literal all-fields/type checking against `Foo`'s fields (missing/extra/mistyped → existing E0009/E0007). Oracle-visible; depends on A1 + A2. |
| **A4** | Manifest correctness + docs + memory | Richer args survive end-to-end into the manifest; verified by `lang-compiler`/`lang-bytecode` unit tests. Docs (§9.13 status), this README, `plans/deferred.md` (move the three items to done), memory. |

## New diagnostic code
- **E0029** — a `#[Foo(…)]` use where `Foo` is not a record/class implementing `Attribute`. (Orphan/unknown standalone-impl target reuses **E0013**; coherence reuses **E0027**.) Next free after this pass: **E0030**.

## Deliberately deferred to pass 2 (recorded, not dropped)
- `attributes_of::<T>()` / `type_of` runtime reflection (the manifest read-back that finally makes manifest content observable to a running program).
- `AttachableTo` / `valid_target` target constraints (constrain *where* an attribute may attach).
- Capability-gated reflection metadata + tree-shaking roots (§9.8.1 / §9.13).
- Standalone impls **with hand-written methods** (e.g. a record implementing `Comparable` with its own `compare`) — needs method-table wiring in both backends; pass 1 covers only empty-body marker impls.
- User-defined derives — out of scope entirely per §9.13.
