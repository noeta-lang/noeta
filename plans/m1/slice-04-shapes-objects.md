# Slice M1.4 — Shapes + objects + enums (the object model)

Status: todo

## Goal
Introduce the shape-based object model (hidden classes, transition tree, flat inline slots, inline caches) and map M0's records/classes/enums onto it.

## Scope
- In:
  - **`lang-object`** crate: shapes describing slot layout; the cached transition tree; flat inline slot arrays; monomorphic **inline caches** at `Member` access and method call sites.
  - Records/classes: instance = header + shape + flat slots; the **all-fields record literal** as the one checked creation primitive (compiler requires every field assigned, in **declared order** so shape identity is deterministic); constructors as ordinary associated functions returning `Self`; methods + `.` access via call-site caches.
  - Enums: a shape per `(enum, variant)` with the discriminant in shape identity and payload in slots; enum-type and record-type used as first-class values become shape-singleton handles.
  - **Structural-update spread** `T { a: 1, ..base }` (shallow), reproducing M0 semantics.
  - Structural equality compares shape + slots (reproducing M0's structural `==`).
- Out: trait-dispatched operators (M1.8); generics specialization beyond shape type-param slots (design only here); packed-value-type flat-array layout (M2).

## Checklist (vertical slice)
- [ ] Grammar / AST: none (reuses M0 `Record`/`Class`/`Enum`/`Object`/`Member`).
- [ ] Checker rule: n/a (M1.7); declared-order full-init is enforced at compile-to-bytecode time as in M0.
- [ ] Bytecode: object-construct (all-fields + spread), member-load/store with IC slots, enum-construct, method-dispatch opcodes.
- [ ] VM op: shape transition, slot resolution, IC fill/hit, enum tag/payload (`lang-vm` + `lang-object`).
- [ ] Conformance cases: existing `records/`, `classes/`, `enums/` cases run on `VmBackend`.
- [ ] Snapshots: disassembly for an object literal, a member access (showing the IC slot), and an enum construct/match.

## Definition of done
- All M0 record/class/enum corpus cases differential-identical on `VmBackend`.
- Two objects of the same type built via the all-fields literal share one shape (declared-order determinism verified).
- miri green; fmt/clippy clean.

## Notes / traps
- M0's per-object `BTreeMap<String, Value>` field bag is **not** the model — that's the naive approach the architecture rejects. Use flat slots keyed by shape.
- The all-fields literal is both the field-init choke point and the shape-identity pin — enforce declared-order slot assignment so canonical shapes are stable.
- This is the hardest Thrust-A slice; it unifies three M0 value kinds (Object/Enum/Type-as-value) onto one representation.
