# lang-object

Shapes: the layout descriptor for the object model.

- **Takes in:** nothing (pure, standard-library-only layout data).
- **Emits:** the `Shape` "hidden class" (type name + ordered slot names + enum variant info) and `ShapeKind` (`Struct`/`Class`/`Opaque`/`Enum`), with `slot_of` for field→slot resolution.

A `Shape` describes the layout of a heap aggregate — a struct/class instance or an enum value. The runtime value (`lang-value`) stores a flat slot array plus a shared `Rc<Shape>` handle, so two aggregates built the same way share one shape rather than each carrying a per-instance field bag (the naive representation the architecture rejects). Shapes are immutable, value-free layout data, so this crate sits *below* `lang-value` in the dependency DAG (`lang-value` depends on it). The compiler emits a flat shape table into the compiled module; the VM wraps each entry in an `Rc<Shape>` once and clones that handle into every value of that shape, making shape identity a cheap pointer comparison.

Inline caches (monomorphic call-site/field-access caches keyed by shape) are a pure performance layer over this representation — invisible in observable output. They are shipped: the VM keeps a per-run cache side-array (sized to `Module.cache_slots`) that stores the last shape pointer seen at each `LoadField`/`CallMethod` site, so a repeated monomorphic access skips the field-name scan / method-table lookup.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
