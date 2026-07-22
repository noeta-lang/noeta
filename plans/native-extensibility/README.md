# Arc — native extensibility: ExtEnum, ExtClass, ExtTrait

Status: **complete** — S1 (ExtEnum) + S2 (ExtClass) + S3 (ExtTrait) all landed

Complete the native extension ABI so a Rust extension can declare real language **enums**, **classes**,
and **traits** — not only opaque extern types (`ExtType`) and modules. Today none of `ExtEnum` /
`ExtClass` / `ExtTrait` exist.

**No deferrals** (explicit product decision, 2026-07-22): the full capability of each ships — including
`ExtClass` with true reference identity + destructor, and `ExtTrait` dynamic dispatch over native
values. Nothing is split into an "A/B, ship-A" shape.

## The unifying model

The language already has user enums (backed `enum X: string`, variants, exhaustiveness), user traits,
and classes/structs, all collected into the checker's symbol tables in `crates/noeta-check/src/collect.rs`.
And `crates/noeta-check/src/prelude.rs` `register_prelude()` already **seeds those same tables from Rust**
by hand (`Ordering`, `Cancelled`, `Attributed<T>`). So each `Ext*` is a *declarative surface over that
seeding* — new `Extension` trait methods (`enums()`/`classes()`/`traits()`) whose seeding runs at prelude
time. Unlike `ExtType` (resolved lazily on lookup), enums/classes/traits must be seeded **eagerly**, because
exhaustiveness (E0011), member access, construction, and bound-checking read those tables directly.

## Cross-cutting requirement — namespace identity (every slice)

Each native-declared type must have the same **qualified identity + `use` projection + scope re-rooting**
as an extern type (the namespaced-types arc + the F5 leaf-module fix `a064078a`). Concretely, in every
slice:
- Key the declaration by qualified `namespace.name` (NOT a bare short name), like `ExtType`.
- Extend `Registry::namespace_types` (currently iterates only `ext.types()`) to include the new kind, so a
  leaf-module `use para.db.SomeEnum` projects — the exact channel the F5 fix opened for extern types.
- Extend `classify_use` (`find_type_qualified`) and `qualified_extern` (`crates/noeta-check/src/stdlib.rs:42`)
  to resolve the new kind.
- Conformance: a consumer imports the native type under a re-rooted scope alias and it unifies by identity.

If this is skipped, `use pkg.SomeType` fails exactly as `db.Connection` did before F5.

## ABI coverage gate (every slice)

`crates/noeta-ext-abi/tests/constraint_fields.rs` + `crates/noeta-embed/tests/ext_constraint_enforcement.rs`
make it a **test failure** to add any `pub` ABI field without classifying it Constraint/Data/Marker/Prose
with a live reader, and — for a Constraint — a live enforcer AND exerciser. Every new struct goes into
`SCANNED`, every pub field into `TABLE`. Constraint fields (`ExtEnum.backing`, `ExtClass` field types /
visibility / mutability, `ExtTrait.methods`) need a **fixture-extension** exerciser in the embed tests
(the corpus runs against std, which won't declare these), mirroring how `ExtTier.sites` /
`ExtDirective.max_args` / `ExtDerive.validate` are exercised today.

---

## S1 — ExtEnum

Native-declared enums: `HttpError.kind()` returns a real enum, `Cookie.with_same_site(SameSite.Lax)`,
exhaustive `match` (E0011). String- and int-backed, and payload-carrying variants.

**Values are real enum values**, not a string shortcut — a `NativeOut::Str` return materializes a
`Value::string`, not an enum, so `match` could never be exhaustive. Ship `NativeOut::Variant { enum_name,
variant, variant_index, fields }` with materializers in BOTH backends. Precedents to generalize (do not
hardcode like `Some/None/Ok/Err`): the role-enum builder `crates/noeta-vm/src/values.rs:275`
(`with_variant_index`), `builtin_enum` in `crates/noeta-eval/src/lib.rs` (~5390), and the recipe paths in
`crates/noeta-vm/src/values.rs` (~599) and `crates/noeta-eval/src/ir.rs` (~1980).

Touches: `crates/noeta-ext-abi/src/registry.rs` (`ExtEnum`, `ExtVariant`, `EnumBacking`, `enums()` on
`Extension`, `NativeValue`/`NativeOut::Variant`); `crates/noeta-check/src/prelude.rs` (`seed_ext_enums`,
modelled on `register_cancelled`/`register_type_enum`); `crates/noeta-check/src/stdlib.rs`
(`qualified_extern`, a prelude-time `sig_to_type`); both backends' materializers; the namespace + gate
cross-cutting work.

- [x] `ExtEnum` ABI + `enums()` seeded into `symbols.enums`/`types`/`type_kinds`
- [x] `NativeOut::Variant` materialized identically in both backends (differential)
- [x] exhaustive `match` passes E0011; non-exhaustive errors E0011
- [x] variant arg IN + variant return OUT; payload-carrying round-trip
- [x] **S1b:** source-level construction (`Hue.Red`, backed `Tone.Warm`, payload `Tag.Labeled(s)`) in both backends
- [x] `backing` enforced (backed `.value()` typing) with a fixture exerciser
- [x] namespace projection: `use pkg.TheEnum` resolves + re-roots
- [x] docs + conformance

### S1 — landed notes (native-extensibility S1)

- **Two identities, deliberately** (mirrors `ExtType`): the **checker** keys the enum by its
  *qualified* `namespace.name` (`symbols.enums`/`Type::Named`, via `qualified_extern`), so it never
  collides with a same-short-named user enum and a native fn's return unifies by identity; the
  **runtime value** carries the *short* name (`NativeOut::Variant.enum_name`, the VM shape name /
  tree-walker `EnumValue.enum_name`). This is required because a source pattern `TheEnum.Variant`
  lowers with `type_name = "TheEnum"` (the short local name — the loader's qualify pass does not
  re-root a native-extension name), and `MatchVariant`/`match_pattern` compare that against the
  value's name. Confirmed by the differential oracle in `crates/noeta-conformance/tests/ext_enum_seam.rs`.
- **`ExtEnum` shape**: `{ name, namespace, variants: &[ExtVariant], backing: EnumBacking }`;
  `ExtVariant { name, fields: &[SigType], value: VariantValue }` — a variant is either
  payload-carrying (`fields`) **or** backed (`value`), never both, like a `.noe` enum.
  `NativeOut::Variant { enum_name, variant, variant_index, fields }` and the arg-in twin
  `NativeValue::Variant { … }` (payload marshalled recursively).
- **`.value()`** on a backed native enum is a real accessor in both backends (registry lookup by the
  value's short name + variant → the declared `VariantValue`); the checker types it off
  `ExtEnum.backing` (`stdlib::native_enum_backing_type`) — the ABI-gate enforcer for that Constraint.
  A non-backed enum has no `.value()`.
- **Source-level construction — WIRED (S1b, native-extensibility):** `SameSite.Lax` (fieldless),
  backed `Tone.Warm.value()`, and payload-carrying `Tag.Labeled(s)` can now be *written in Noeta
  source* and construct a real enum value — usable in a `match`, `==`-comparable, and passable into
  native code — not only received from a native call. Seeded through the **same channel `Ordering`
  uses**, gated by the `use pkg.TheEnum` import (namespace identity preserved):
  - **Bytecode compiler** (`register_types`, `crates/noeta-compiler/src/lib.rs`): the `Stmt::Use`
    arm now inspects `classify_use`; a `UseKind::ExtEnum(qualified)` seeds `self.types` with a
    `TypeInfo::Enum` (via `ext_enum_type_info`) keyed by the imported **short** name, so `Hue.Red` /
    `Hue.Labeled(x)` lower to `MakeEnum` exactly like a `.noe` enum. Variant order (hence index)
    comes from the registry; a payload variant's field names are positional placeholders (`_0`, …)
    — only their **count** is load-bearing (gates `lower_field`'s payload-vs-fieldless split and the
    `MakeEnum` arity), and enum `==`/match compare by name+variant+arity, never field name, so the
    shape stays identical to the native-return path's empty-name shape.
  - **Tree-walker** (`declare_use`, `crates/noeta-eval/src/lib.rs`): a `UseKind::ExtEnum` import now
    binds the short name to a real `Value::EnumType(EnumDef)` built from the registry (was an opaque
    `Value::Type`), so `read_member`/`make_variant` construct the same value. `.value()` still
    resolves off the registry by the value's short name (S1).
  - **Checker** (`enum_type_key`, `crates/noeta-check/src/expr/patterns.rs`, used by the two
    construction sites in `member.rs`/`calls.rs`): a native enum is seeded under its *qualified* name
    only, so `is_enum_variant`/`enum_construction_type` first resolve the source-written short name
    through the `use`-import `extern_types` alias to the qualified key. Construction then yields
    `Type::Named(qualified)`, which unifies with a native fn's parameter type by identity. A user
    enum of the same short name shadows the import (direct hit wins).
  - **Differential:** `crates/noeta-conformance/tests/ext_enum_seam.rs::native_enum_source_construction_round_trips_identically_on_both_backends`
    — construct fieldless/backed/payload variants in source, match, `==`, `.value()`, and pass into
    native code; asserts `reference_run == VmBackend::run_module` + exact stdout (gates verified).
- **ABI gate:** `ExtEnum`/`ExtVariant` added to `SCANNED`; `ExtEnum.backing` is a Constraint
  (enforcer `native_enum_backing_type`, exerciser
  `ext_constraint_enforcement.rs::a_backed_ext_enum_value_type_is_enforced`); the remaining fields are
  Data with live readers.

## S2 — ExtClass (a TRUE reference type)

Native-declared **classes**: reference identity, a **destructor** (native drop on collection), full
participation in the RC + cycle collector, native state, AND language-visible fields the language reads and
constructs. **NOT `NativeOut::Struct`** — that produces a value struct with no identity or destructor, which
would be a struct wearing a class's name and could hold no resource needing cleanup.

The representation decision must be made **deliberately and reviewed**, not defaulted:
- (i) a language `Object` (`Payload::Object`) the RC/cycle collector manages uniformly, plus an optional
  native drop hook + native methods; vs
- (ii) an extern-box (`ExternValue`, its own `Drop`) that the checker types as a fielded class (field access
  projects onto the box).
Map both; pick the one that gives real class semantics (identity + destructor + cycle participation) with
the least duplicated machinery. `ExtClass` is `ExtType` grown up — the same reference-identity family, plus
fields + construction + destructor — not a struct.

- [x] representation decided + written down (identity, destructor, cycle participation proven)
- [x] language constructs a native class; fields read/(mut) per declared visibility
- [x] destructor runs on collection (leak oracle zero; a native-drop side effect observed)
- [x] reference identity (two bindings alias; `==` is identity)
- [x] namespace projection + gate + docs + conformance

### S2 — landed notes (native-extensibility S2)

- **Representation chosen: (i) a real `Payload::Object` with `ShapeKind::Class`** (reviewed and
  approved). Option (ii) — an extern-box typed as a fielded class — was disqualified on
  *correctness*, not cost: an extern box is a GC **leaf** (`heap::children`'s `Payload::Extern` arm
  yields nothing), so the cycle collector cannot trace *through* it; language-value fields would have
  to be arena `Retained` entries, which the arena treats as **roots** (never collected) and which the
  box's ctx-less `Drop` cannot release. Option (i) inherits identity, reference/aliasing semantics,
  RC, and cycle participation from the object model unchanged; the only new runtime bit is
  materializing a class-kind object from the carrier.
- **`NativeOut::Instance { class, fields }`** (+ the arg-IN twin `NativeValue::Instance`) is the new
  carrier — deliberately **not** an overload of `NativeOut::Struct` (which materializes
  `ShapeKind::Struct`, a *value* struct, leaving that path untouched). Both backends materialize it
  into a real class-kind `Object` (`Shape::object(ShapeKind::Class, …)` → `structural_eq = false` →
  `==` is identity): VM `materialize_native` (`crates/noeta-vm/src/values.rs`), tree-walker
  `materialize_native` (`crates/noeta-eval/src/lib.rs`, a fresh `TypeDef { is_struct:false,
  structural_eq:false, destructor:None }`). Fields materialize recursively in declared slot order.
- **Destructor = RAII on an extern-handle field** (the approved shape; no host-coupled finalizer
  built). Native state lives in a field typed as an `ExtType` whose `ExternValue` has a Rust `Drop`;
  when the object frees, the field's box drops and `Drop` runs — verified to fire on **both** paths:
  a last-reference release *and* destructor-free cycle reclamation. The mechanism is `heap::free`
  reconstructing and `drop`ping the `Box<Obj>` (so the `Payload::Extern` box always drops), on both
  the `Trace` and `TrialDeletion`/exit-reaper paths.
  - **Not seeded into `destructor_classes`** (deliberate — the plan listed it, but a native class has
    no `.noe` `destruct` block): the cleanup is the field's `Drop`, which the collector runs
    unconditionally. Seeding it would falsely claim a language destructor and *defer* the class's
    destructor-free cycles to the exit reaper instead of reclaiming them mid-run; leaving it out keeps
    mid-run reclamation and the `Drop` fires on every free path regardless.
- **Seeding — `seed_ext_classes`** (`crates/noeta-check/src/prelude.rs`) mirrors
  `register_extension_attributes` + the class-only tables the collect pass writes: `records` (field
  types via `sig_to_type`), `type_kinds = Class`, `private_fields` (E0035), `mut_fields` (E0033),
  keyed by **qualified identity** (`fx.Handle`). Source construction resolves the short name through
  the `use`-import alias to the qualified `records` key (`synth_object_named`); backend source
  construction seeds a class-kind `TypeInfo::Class` (bytecode) / `Value::Type` `TypeDef` (tree-walker)
  under the imported short name, mirroring S1b's enum construction.
- **Namespace cross-cut** (identical to S1): `UseKind::ExtClass`, `namespace_types`/`classify_use`/
  `resolve_namespace_child`/`qualified_extern` extended for `classes()`; `use pkg.TheClass` re-roots
  and unifies a native fn's `Handle` return by identity.
- **ABI gate:** `ExtClass`/`ExtField` in `SCANNED` + `TABLE`; `ExtField.is_public` (E0035) and
  `is_mut` (E0033) are **Constraints** exercised by fixtures in `ext_constraint_enforcement.rs`
  (`a_native_class_field_visibility_is_enforced` / `a_native_class_field_mutability_is_enforced`); the
  rest are Data with live readers.
- **Conformance:** `crates/noeta-conformance/tests/ext_class_seam.rs` (own fixture extension, std
  declares no class) — the differential proves construction (native + source), field read/mutate,
  reference identity (`==`/aliasing), and arg-IN agree on both backends; two leak-oracle-zero tests
  prove the destructor fires on linear collection **and** on mutual-reference cycle reclamation (both
  guards' `Drop`), the load-bearing cases that distinguish a true class from a struct. All cases
  gate-verified (mutate an expect → fail → revert). Gates green: corpus 7/7, `noeta-check`/`noeta-vm`/
  `noeta-eval` `--lib`, `constraint_fields` + `ext_constraint_enforcement`, fmt, workspace clippy
  `-D warnings`.

## S3 — ExtTrait (contract AND dynamic dispatch)

Native-declared traits, slotting into the **user-trait** machinery (`symbols.user_traits` /
`user_trait_impls`; `satisfies_user_trait`, `enforce_type_param_bounds`, `check_user_trait_impl`) — NOT the
closed `BuiltinTrait` enum.

**3a — contract for user types (tractable):** synthesize a `noeta_ast::TraitDecl` from the ABI declaration
(needs a new `SigType → TypeRef` reverse map for method sigs); `impl NativeTrait for UserType`, `T:
NativeTrait` bounds, incomplete-impl E0015. Clean extension of the working user-trait path.

**3b — dynamic dispatch over native values (the runtime bridge, NOT deferred):** `dyn NativeTrait` holding a
**native** value, calling a trait method. Today user-trait `dyn` dispatch resolves to a hoisted Noeta body;
a native value's methods live behind `ExtType::dispatch` (`TypeDispatch` fn pointer). This slice builds the
bridge from the user-trait dynamic-dispatch site to the native dispatch seam in **both** backends
(`crates/noeta-eval/src/lib.rs:5037`, `crates/noeta-vm/src/values.rs:370`). This is genuine new runtime
plumbing — treat it as its own sub-slice with its own differential conformance.

- [x] 3a: `impl NativeTrait for UserType` + `T: NativeTrait` bound (satisfied / E0015 / E0025)
- [x] 3b: `dyn NativeTrait` over a native receiver dispatches to native methods, both backends, differential
- [x] namespace projection + gate + docs + conformance

### S3 — landed notes (native-extensibility S3)

- **Keyed by short name, gated by the `use`** (deliberately *unlike* S1/S2's qualified keying). The
  user-trait machinery is short-name-keyed everywhere from source — `impl Widget for T`, `T: Widget`,
  `dyn Widget` all name the trait by the imported spelling (exactly like a `.noe` trait and the
  built-in traits). So a native trait seeds `symbols.user_traits` under the **imported short** name;
  the `use fx.Widget` alias in `imports.extern_types` gates it (bare `impl Widget` without the `use`
  resolves nothing). `ExtEnum`/`ExtClass` key by *qualified* identity because their **values** must
  unify; a trait is a contract, not a value.
- **`seed_ext_traits`** (`crates/noeta-check/src/prelude.rs`) runs at **collect time** (import-aware),
  called from `collect.rs` **after** the `Stmt::Trait` walk and **before** the `user_trait_impls`
  collection. It iterates `imports.extern_types` for aliases resolving to a native trait, synthesizes
  a `noeta_ast::TraitDecl` (`synth_trait_decl`), and `.or_insert`s it — so a user `trait Widget`
  (collected first) **shadows** the same-named native trait (the S1/S2 "user shadows native" rule;
  the plan's flagged shadow-ordering fork, resolved this way rather than native-wins).
- **`SigType → TypeRef` reverse map** (`stdlib::sig_to_typeref` / `ret_to_typeref`): a `TraitDecl`'s
  method sigs carry AST `TypeRef` (not lattice `Type`), because `check_user_trait_impl` (E0015) and
  the `dyn`-method result typing (`member.rs`) read them through `field_type`. Primitive spellings
  round-trip through `Type::from_ref`; a `SigType::Named` bakes its qualified identity so the declared
  type resolves by identity regardless of alias presence; a variable/polymorphic form → permissive
  `dyn` hole.
- **3b — dynamic dispatch is the *existing* extern-method seam, ZERO runtime surgery** (Option A,
  reviewed/approved). A native value behind `dyn NativeTrait` is an `ExtType` (extern-box) value; a
  method call on it lowers to an ordinary method call, and **both backends already route an extern
  receiver to native dispatch** keyed off the runtime value, not the static `dyn` type: tree-walker
  `call_method` → `call_extern_method` (`crates/noeta-eval/src/lib.rs`), VM `Op::CallMethod`'s
  `HeapKind::Extern` arm → `resolve_extern_route`/`call_extern_method` (`crates/noeta-vm/src/dispatch.rs`,
  `methods.rs`). **No Object-arm change.** The only bridge is the checker **coercion channel**:
  `seed_ext_traits` seeds `user_trait_impls[native_type_qualified][short_trait]` for each native type
  advertising the trait in its existing `ExtType.traits` list (a non-built-in name there, which
  `record_trait_impls` otherwise drops), so `assignable`/`type_impls_trait` coerces `Type::Named("fx.Button")`
  → `dyn Widget`. The advertiser loop is written over a generic type source, so a future ExtClass with
  a `traits` field joins it without redesign (Option B — dispatch over an ExtClass `Value::Object`
  receiver — deferred, a separate Object-arm decision).
- **Namespace cross-cut** (mirrors S1/S2): `UseKind::ExtTrait`, `namespace_types` / `classify_use` /
  `resolve_namespace_child` / `qualified_extern` extended for `traits()`; `use fx.Widget` re-roots the
  short name onto its qualified identity.
- **ABI gate:** `ExtTrait`/`ExtTraitMethod` in `SCANNED` + `TABLE`; `ExtTrait.methods` is the
  **Constraint** (enforcer `check_user_trait_impl` E0015, exerciser
  `ext_constraint_enforcement.rs::a_native_trait_incomplete_impl_is_rejected`); the rest are Data with
  live readers.
- **Conformance:** `crates/noeta-conformance/tests/ext_trait_seam.rs` (own fixture extension) — the
  differential proves a user `impl` + a `T: Widget` bound + a `dyn Widget` dispatching to a `.noe`
  body (Card) AND to the **native** method (Button) agree on both backends, leak-oracle zero; two
  check-only tests pin the incomplete-impl E0015 and the bound-violation E0025. All cases
  gate-verified (mutate expect → fail → revert). Gates green: corpus 7/7, `noeta-check`/`noeta-vm`/
  `noeta-eval` `--lib`, `noeta-ext-abi` `--lib`, `constraint_fields` + `ext_constraint_enforcement`,
  workspace clippy `-D warnings`.

## Definition of done
All three declaration kinds usable from a consuming project with correct namespace identity; enums exhaustive;
classes have identity + destructor + cycle participation; traits both user-impl'able and dynamically
dispatchable over native values; differential oracle + leak oracle green; ABI coverage gate satisfied per
new field; conformance per slice.

This arc settles the "does std need a `.noe` layer?" question for type **declaration** — the residual is
Noeta method *bodies* only. Record that in `backlog.md` on completion.
