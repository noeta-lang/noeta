# noeta-value

The M1 runtime value: a NaN-boxed 64-bit word, and its operator semantics.

- **Takes in:** `BinaryOp`/`UnaryOp` (from `noeta-ast`), `DiagnosticCode` (from `noeta-diagnostics`), `Shape` (from `noeta-object`).
- **Emits:** `Value` (immediate unit/bool/int/float + heap-pointer strings, boxed i64, closures, lists/maps, and shaped objects/enums), the refcount primitives (`inc_ref`/`dec_ref`/`free`, which release a collection's or object's elements recursively), the cycle-collector mechanism the `noeta-gc` `CycleCollector` drives (`Color`, per-object color/buffered flags, raw non-freeing refcount edits, child enumeration, a child-preserving `free_shallow`, and the `set_slot` mutation primitive), `apply_binary`/`apply_unary` (mirroring the M0 tree-walker exactly, including structural object/enum equality), and `compare_primitive`/`structural_compare` (the primitive total order behind `.compare()`, `< <= > >=`, `sorted`, `min`/`max` and a set's canonical buffer, plus the field-wise object ordering `@derive(Comparable)` synthesizes — one comparator, so no two of those doors can disagree).

It also owns the heap-side measurements the memory oracles read, because the allocation and free paths are the only places that can take them: live-object residency and its peak, the refcount-anomaly count the cycle collector notes, and the **skipped-destructor audit** (`destruct_audit_begin`/`note_destructor_run`/`destruct_audit_end`) — objects allocated with a destructor-bearing shape weighed against destructors the runtime actually ran, which is how an object freed with its `destruct` never run is caught. The audit is inert until armed.

This crate carries `unsafe` — the NaN-box pointer round-trip (via exposed provenance) and heap-header access, kept small and `miri`-gated — so it deliberately does not inherit the workspace `unsafe_code = "forbid"` lint.

It is no longer the only one. `noeta-vm`, `noeta-jit`, `noeta-jit-abi`, `noeta-db`, `noeta-host-real`, `noeta-playground`, `noeta-alloc-probe`, `noeta-aot-runtime` and the wasm crates each opt out for their own reason. **Do not read this paragraph as an audit boundary**: the authoritative list is the set of crates opting out of the workspace lint (`grep -l unsafe_code crates/*/Cargo.toml`), because that opt-out is what a new `unsafe` block requires and is therefore the thing that cannot drift silently. This sentence once said "the one crate with `unsafe`" and stayed that way long after it stopped being true.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
