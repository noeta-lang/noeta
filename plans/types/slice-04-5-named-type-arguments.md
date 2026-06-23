# Slice S4.5 — `Type::Named` carries type arguments (precise instance typing)

Status: **done**

> **Track:** inferred-static type system. **Closes** the third S4 follow-up gap recorded in `plans/deferred.md`: a generic container losing its element type through an instance. **Determinism / oracle posture:** front-end only — `Type` is a checker construct (the runtime uses `Value`), so no backend changes; differential holds at **0 skipped**.

## What shipped

`Type::Named(String)` → `Type::Named(String, Vec<Type>)`. A generic instance now keeps its arguments, so they flow back out:

- **`from_ref`** builds the arguments from a `TypeRef`'s generic arguments; **Display** renders `Box<int>`; **`subtype`** is covariant in the arguments, with an **empty argument list treated as "unspecified"** (compatible with any instantiation of the same name — a literal or partially-erased instance); **`erase_type_params`**, **`apply_subst`**, and **`bind_type_params`** recurse through the arguments (a bare parameter `T` still erases to `dyn` / resolves via the substitution; a named generic `Box<T>` recurses).
- **Constructors carry precise arguments**: `Box.new(1)` instantiates `T=int` and returns `Box<int>` (the substituted, un-erased return type).
- **Instance method and field access seed the substitution from the receiver's arguments**: `b: Box<int>` makes `b.get(): int` and `b.value: int`, not `dyn`. A new `generic_types` map (type name → ordered parameter names) drives the field-access substitution; `check_generic_call` gained a `recv_args` parameter that seeds the class parameters before the call's own arguments refine them. An unresolved parameter (arguments unknown) **erases to `dyn`** rather than leaking the parameter name (which would otherwise fail an operator-trait check).
- **Instance-method bound enforcement** rides the same seeding (belt-and-suspenders over construction enforcement, which already guarantees a constructed instance's parameter satisfies its bounds).

## Boundary kept (recorded in deferred.md, not dropped)

A **record/object literal** does not yet infer its arguments from field values: `Box { value: 1 }` types as `Box` with *unspecified* arguments (lenient, not precise). Constructors via an associated `fn` are precise. Inferring a literal's arguments (unify each field value against the field's declared parameter) is the noted follow-up.

## Files

- `crates/lang-types/src/lib.rs` — `Named(String, Vec<Type>)`; `from_ref`, `Display`, `subtype` (covariant + empty-as-wildcard); test constructors.
- `crates/lang-check/src/lib.rs` — every `Named` match/construction site; `generic_types` field + population; `recv_args` seeding in `check_generic_call`/`call_user_method`; field-access substitution; `erase`/`apply_subst`/`bind` argument recursion.
- `crates/lang-check/src/stdlib.rs` — 4 `Named` sites.
- `tests/conformance/generics/` — `instance_type_argument_tracked.lang` (runs, precise), `instance_type_argument_mismatch.lang` (`E0007`).
- `crates/lang-check/src/tests.rs` — `instance_keeps_its_type_argument`.

## Verification

Conformance **146 / differential 132 matched / 0 skipped / backends agree**; workspace tests, clippy, fmt clean.
