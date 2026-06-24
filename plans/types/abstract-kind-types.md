# Abstract kind-types: `Enum` · `Record` · `Class`

Status: **DONE** (conformance 203 / differential 197 / 0-skipped / backends agree / miri-clean).
Branch `types-inferred-static`. Foundation for the
[semantic-roles](../attributes/semantic-roles.md) rework (`role: Enum`); built as a full family up
front (no deferral, per the design discussion).

**As built:** `Type::Kind(TypeKind)` (one lattice variant, `TypeKind = Enum|Record|Class`); pure
`Type::subtype` handles `Kind(k) <: dyn`/`Kind(a) <: Kind(b)`, while the registry-dependent
`Named(n) <: Kind(k)` membership lives in a new kind-aware checker funnel `Checker::assignable`
(+ `arg_assignable`, routing `subsume` and all four argument-check sites) backed by a `type_kinds`
registry populated in `collect` and for the prelude types. `Enum`/`Record`/`Class` parse as built-in
type names (`is_builtin_name` + `from_ref`); the `as`-gate accepts a `Kind` source; `match` over a
`Kind` value needs no checker change (it falls through the existing lenient non-enum path + runtime
backstop, exactly like `dyn`). Runtime kind tests (`is`/`.as<>()`) are one new `NarrowTarget`
family (`AnyEnum`/`AnyRecord`/`AnyClass`) keyed on the value's shape kind (VM `narrow_matches`) /
`Value::Enum` + `TypeDef::is_record` (eval `runtime_matches`) — both backends agree by construction.
No new diagnostic code. Commit pending.

## What and why

Introduce three **abstract supertypes** — `Enum`, `Record`, `Class` — one per declared-type kind.
Every declared enum is a subtype of `Enum`, every record of `Record`, every class of `Class`, and
all three widen to `dyn`. This is the PHP `UnitEnum` / Java `java.lang.Enum` / C# `System.Enum`
model, generalized to the three nominal kinds the language has.

The immediate consumer is `roles_of()`, whose `role` field is "some enum" (a heterogeneous mix of
`@semantic` enums) — exactly `Enum`, and strictly more honest than `dyn`, which would throw away
what we statically know. The family (`Record`/`Class`) is symmetric and falls out of the same
machinery; we build all three so the type system is complete rather than enum-only.

These are **abstract**: no value has an abstract type at runtime (every value is a concrete
enum/record/class). They appear only in *static* positions — a field type, a parameter, a return
type — as a bound weaker than a concrete type but stronger than `dyn`.

```
fn audit(role: Enum) { ... }              // accepts any enum value
field handler: Class                       // any class instance
for b in roles_of() { b.role /* : Enum */ }
```

## Semantics

- **Subtyping.** `Named(n) <: Enum` iff `n` is a declared enum; likewise `Record`/`Class`. All three
  `<: dyn`; each is reflexive; they are mutually unrelated (`Enum` is not `<: Record`). A concrete
  enum is **not** a supertype of `Enum` (you narrow to go the other way).
- **Match** over a value typed `Enum` is an **open** domain (you do not statically know *which*
  enum), so it requires a `_` arm — same posture as `dyn`/an open `match`. Optional follow-up:
  flow-narrow `x` to the concrete enum inside an arm matched by `SomeEnum.Variant`.
- **Narrowing.** `x.as<WebRole>()` and `x is WebRole` are valid from an `Enum`/`Record`/`Class`
  source (they join `dyn`/union as narrowable sources; narrowing a *concrete* type stays `E0028`).
  And `v.as<Enum>()` / `v is Enum` from a `dyn` value is a **runtime kind test** — "is this value
  any enum" — answered structurally in both backends (the value is an `Enum`/`Record`/`Class`
  shape), so the abstract types are first-class narrow targets too.

## Mechanism

- **Lattice** (`lang-types`): one new variant `Type::Kind(TypeKind)` where `TypeKind = Enum | Record
  | Class` (a small enum local to `lang-types`). One variant keeps the lattice tidy versus three
  parallel ones. `Type::subtype` (pure, no registry) handles only the registry-free cases:
  `Kind(k) <: dyn`, `Kind(k) <: Kind(k)`, and the hole/`Unknown` rules.
- **Kind membership in the checker** (`lang-check`): the registry-dependent rule `Named(n) <:
  Kind(k)` lives in **`subsume`** (line ~1555 — the single assignability funnel, which has
  `self.enums`/`self.records` and the type-kind information), *not* in pure `subtype`. This needs the
  checker to distinguish record-kind from class-kind `Named`s; today both live in `self.records`, so
  add a `self.type_kinds: HashMap<String, TypeKind>` populated in `collect` (or reuse
  `reflect::TypeInfo.kind`). `Enum` membership already has `self.enums`.
- **Parser/type grammar** (`lang-parser` + `Type::is_builtin_name` / `from_ref`): recognize `Enum`,
  `Record`, `Class` as built-in type names → `Type::Kind(...)`. **Reserves these three as built-in
  type names** — a user `type Record = {...}` would now collide (call out in docs; they were always
  poor type names). `TypeRef` → `Type` conversion maps them.
- **Narrowing/`is`** (S6/S8 paths): add `Kind(_)` to the set of narrowable `as`/`is` sources
  (`lang-check` gate), and a runtime "is value of kind k" check in `narrow_matches` (VM) +
  `runtime_matches` (eval), keyed on the existing shape-kind / `Value::Enum`-vs-`Object`
  classification both backends already expose. (Reuses the `NarrowTarget` plumbing; one new target
  shape "any of kind k".)
- **`type_of`/reflection** is unaffected: a runtime value's type is always its *concrete* kind
  (`Named("WebRole")`), so the `Type` ADT needs no abstract-kind variant; the abstract types are a
  static-only concern.

## Diagnostics

No new code expected — assignability failures are `E0007`, narrowing rules reuse `E0028`, unknown
names `E0013`. (Confirm during implementation that no edge wants a dedicated code.)

## Touch list

- `lang-types`: `Type::Kind(TypeKind)` + `TypeKind`; `subtype` arms; `is_builtin_name`; display.
- `lang-check`: `subsume` kind-membership rule; `self.type_kinds` registry; `check_type_ref` accepts
  the three names; narrow-source gate; (optional) arm flow-narrowing.
- `lang-eval` + `lang-vm`: runtime kind test in `runtime_matches` / `narrow_matches` for
  `is Enum`/`.as<Enum>()` against a `dyn` value.
- `lang-parser`: the three names parse as types (mostly via `is_builtin_name`); a snapshot.
- Conformance `tests/conformance/types/`: `Enum`/`Record`/`Class` as a parameter/field type; a
  concrete enum assignable to `Enum` but not to `Record`; `dyn`→`Enum` via `is`/`as`; a `match` over
  an `Enum`-typed value requiring `_`; differential.
- Checker units; docs (`docs/resources/02-syntax.md` type section, `01-architecture.md` lattice).

## Verification

Conformance + differential (0-skipped, backends agree), workspace tests, clippy, fmt, miri (the new
runtime kind test). Branch `types-inferred-static`; standard trailers.

## Sequencing

Build **first** — [semantic-roles](../attributes/semantic-roles.md) depends on `Enum`. The
structured-args and generic-derives plans are independent of this one.
