# Arc — native extensibility: ExtEnum, ExtClass, ExtTrait

Status: **in progress** — S1 (ExtEnum) complete; S2/S3 pending

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
- **Scope boundary (decision, plan left open):** S1 delivers the full **round-trip** (native returns
  a variant → user `match`es it → user passes it back into native code) and `.value()`. **Source-level
  construction of a native enum variant** (`SameSite.Lax` written in Noeta) is *not* wired: the
  bytecode compiler builds its `TypeInfo::Enum` table from the program AST only (like it seeds
  `Ordering` by hand), so a native enum name is not a constructible type handle at a call site. This
  was never in the S1 checklist (which says "variant arg IN + variant return OUT; **round-trip**")
  and the motivating `Cookie.with_same_site(SameSite.Lax)` needs `ExtClass Cookie` (S2) anyway. If a
  future slice wants source construction, seed the compiler's `types` map + the tree-walker scope
  from the registry (the same channel `Ordering` uses).
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

- [ ] representation decided + written down (identity, destructor, cycle participation proven)
- [ ] language constructs a native class; fields read/(mut) per declared visibility
- [ ] destructor runs on collection (leak oracle zero; a native-drop side effect observed)
- [ ] reference identity (two bindings alias; `==` is identity)
- [ ] namespace projection + gate + docs + conformance

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

- [ ] 3a: `impl NativeTrait for UserType` + `T: NativeTrait` bound (satisfied / E0015 / E0025)
- [ ] 3b: `dyn NativeTrait` over a native receiver dispatches to native methods, both backends, differential
- [ ] namespace projection + gate + docs + conformance

## Definition of done
All three declaration kinds usable from a consuming project with correct namespace identity; enums exhaustive;
classes have identity + destructor + cycle participation; traits both user-impl'able and dynamically
dispatchable over native values; differential oracle + leak oracle green; ABI coverage gate satisfied per
new field; conformance per slice.

This arc settles the "does std need a `.noe` layer?" question for type **declaration** — the residual is
Noeta method *bodies* only. Record that in `backlog.md` on completion.
