# Slice M1.4 — Shapes + objects + enums (the object model)

Status: done

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
- [x] Grammar / AST: none (reuses M0 `Record`/`Class`/`Enum`/`Object`/`Member`).
- [x] Checker rule: n/a (M1.7); full-init (E0009) enforced at runtime by `MakeRecord`, as in M0.
- [x] Bytecode: object-construct (`MakeRecord` all-fields + spread, `MakeOpaque`), member-load (`LoadField`), enum-construct (`MakeEnum`), method-dispatch (`CallMethod`).
- [x] VM op: shared `Rc<Shape>` handles, slot resolution, enum tag/payload, instance-method dispatch (`lang-vm` + `lang-object`).
- [x] Conformance cases: `records/record_literal`, `classes/missing_field`, `classes/structural_update`, `modules/namespace_and_use` run on `VmBackend` (the `enums/` + `classes/construct_and_method` cases also need `match`, landing in M1.5).
- [x] Snapshots: disassembly of the object model (record + class with constructor/method, shape & method tables, field loads, enum construct).

## Definition of done
- [x] All M0 record/class corpus cases differential-identical on `VmBackend` (enum-`match` cases deferred to M1.5, which reaches the 100% Thrust-A gate).
- [x] Two objects of the same type built via the all-fields literal share one shape (interned `Rc<Shape>`, declared-order determinism verified by unit test).
- [x] miri green; fmt/clippy clean.

## Notes / traps
- M0's per-object `BTreeMap<String, Value>` field bag is **not** the model — that's the naive approach the architecture rejects. Use flat slots keyed by shape.
- The all-fields literal is both the field-init choke point and the shape-identity pin — enforce declared-order slot assignment so canonical shapes are stable.
- This is the hardest Thrust-A slice; it unifies three M0 value kinds (Object/Enum/Type-as-value) onto one representation.

## Outcome

Landed the shape-based object model, lifting VM corpus coverage **62.5% → 75.0%** (20 → 24 cases matched), zero divergence. Newly covered: `records/record_literal`, `classes/missing_field`, `classes/structural_update`, `modules/namespace_and_use`. (The enum/`match` cases — `enums/*`, `classes/construct_and_method`, `results/*`, the §14 demo — are unblocked but await `match`/`?`/`??` in M1.5.)

**`lang-object` (new crate).** Pure layout metadata: `Shape { kind, name, fields, variant, builtin_result_option }` + `ShapeKind {Record, Class, Opaque, Enum}`, with `slot_of`. No runtime `Value` — it sits below `lang-value`, which depends on it. (The architecture's notional "object → value" dependency is inverted here: shapes are pure data and `Value::display` needs them self-contained, so `Value` holds an `Rc<Shape>`. Noted in `AGENTS.md`.)

**Value representation (`lang-value`).** Two heap payloads, `Object { shape: Rc<Shape>, slots }` and `Enum { shape: Rc<Shape>, data }` — a flat slot array plus a *shared* shape handle (equal-built aggregates point at one `Rc`, so identity is a pointer comparison; the slice's determinism guarantee). Each owns one reference per slot/datum; `heap::free` releases them recursively. Display mirrors M0 exactly: objects `Type {field: repr, ...}` in slot order, enums `Type.Variant(data...)` (or bare `Ok(x)`/`none` for built-ins) with data shown via `display`. Structural equality (`ops::values_equal`) compares type name + fields + slots (objects) / enum name + variant + data (enums).

**Bytecode (`lang-bytecode`).** The `Module` gains a `shapes` table (interned, referenced by index) and a `methods` dispatch table (`MethodEntry`). New ops: `MakeRecord` (declared-type construct with `..spread` and runtime E0009 missing-field check), `MakeOpaque` (sorted-key construct for `use` stubs), `MakeEnum`, `LoadField`, and a generalized `CallMethod` (replacing M1.3's `Method`-enum op) that dispatches user instance methods *or* the `count`/`enumerate` built-ins at runtime.

**Lowering (`lang-compiler`).** A three-pass compile: (1) register every top-level `type`/`class`/`enum`/`use` and reserve a prototype per class `fn`; (2) compile method bodies (receiver in register 0, declared params after, field names resolving to the receiver via `LoadField`); (3) compile the top-level program. `Type.f(args)` resolves at compile time to an associated-function call (unit self) or an enum-variant construction; an instance `obj.f(args)` lowers to runtime-dispatched `CallMethod`. A class `fn` compiles to one prototype serving both call forms.

**Execution (`lang-vm`).** Builds one `Rc<Shape>` per shape-table entry and a `(type, method) → proto` map at startup. `CallMethod` on an object pushes a `[recv, args...]` frame through the method table; `MakeRecord` assembles slots from spread + named initializers and raises E0009 (releasing partial slots first) if a declared field is unset; `MakeOpaque` builds a fresh sorted-key shape. Refcount discipline extends to slots/data and the spread/missing-field paths — miri green.

**Conservative skips (kept faithful):** a data-carrying enum variant used without arguments, a bare associated-function-as-value, and an enum used as a record literal remain unsupported (no corpus case exercises them; the tree-walker handles them but they are out of this subset). Inline caches are a documented perf-only deferral — RunResult-invisible — so member access is a direct shape lookup.
