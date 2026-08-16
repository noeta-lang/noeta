# noeta-object

Shapes: the layout descriptor for the object model.

- **Takes in:** nothing (pure, standard-library-only layout data).
- **Emits:** the `Shape` "hidden class" (type name + ordered slot names + enum variant info) and `ShapeKind` (`Struct`/`Class`/`Opaque`/`Enum`), with `slot_of` for field→slot resolution.

Beside the layout a shape carries the per-type metadata the runtime has nothing else to consult for — whether `==` is structural, whether values may key a map, which slots the deep marshal omits, and which slots are declared `u64` and so order unsigned. That last one is why a `@derive(Comparable)` type with a `u64` field orders by its value everywhere, `dyn` launder included: a fixed-width integer is erased to its i64 word, and a field's signedness is a property of the type rather than of any site that compares it. Metadata is excluded from a shape's equality and hashing, so a shape built without declaration context still resolves to the compiler's interned one.

A `Shape` describes the layout of a heap aggregate — a struct/class instance or an enum value. The runtime value (`noeta-value`) stores a flat slot array plus a shared `&'static Shape` handle, so two aggregates built the same way share one shape rather than each carrying a per-instance field bag (the naive representation the architecture rejects). Shapes are immutable, value-free layout data, so this crate sits *below* `noeta-value` in the dependency DAG (`noeta-value` depends on it). The compiler emits a flat shape table into the compiled module; the VM [interns](intern_shape) each entry once (P-PAR S1b) and clones that `Copy` `&'static Shape` handle into every value of that shape, making shape identity a cheap pointer comparison with zero refcount traffic — and, being `'static` of a `Sync` type, shareable with other isolate threads for free.

Inline caches (monomorphic call-site/field-access caches keyed by shape) are a pure performance layer over this representation — invisible in observable output. They are shipped: the VM keeps a per-run cache side-array (sized to `Module.cache_slots`) that stores the last shape pointer seen at each `LoadField`/`CallMethod` site, so a repeated monomorphic access skips the field-name scan / method-table lookup.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
