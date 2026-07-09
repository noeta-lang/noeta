# Kernel methods — extension method bundles for packed types

*Status: **✅ K0–K6 COMPLETE (2026-07-09, branch `kernel-methods`)** — bundles registered
(`ExtBundle` on `ExtModule.bundles`), bound (`impl vec.Kernels for T {}`, impl-site constraint
validation), typed + lowered (call-site-resolved `Op::BundleMethod` — a deliberate deviation from
this plan's runtime shape-keyed table: an empty list receiver works, dispatch costs nothing, `dyn`
documented as unreachable; a runtime table stays additive), dispatched by both backends through
the shared `ctx_receiver_call` shape, surfaced in member completion, dogfooded (`vec.Kernels`
Element + Bulk), perf-gated at parity (method ≡ module, −0.3%/−2.1%), and proven third-party
through toolchain composition (`fx.Pixels` in the composed e2e). Follow-on (std.vec hybrid
package with canonical types) below remains future work.*

## Motivation

Package-manager N3.4 delivered the raw-buffer **capability**: a native function can borrow a
packed list's contiguous bytes (`with_packed` / `with_packed_mut` / `make_packed_like`, the neutral
`PackedView`, `NativeOut::Scalars`). But the **surface** is free module functions —
`vec.dot_all(xs, ys)` — connected to the user's `@packed` type by nothing but memory layout:

- the checker cannot verify the shape requirement (the signature says `Dyn`; a wrong-shaped
  argument fails at dispatch, not statically);
- the LSP has nothing to offer in member position (`ps.` lists only user-declared methods);
- the operations don't read as belonging to the data (`vec.dot_all(xs, ys)` vs `xs.dot_all(ys)`).

The original vision (recorded 2026-07-09) was PHP-trait-shaped: a type **opts into a bundle of
kernel methods**, explicitly, and from then on the whole toolchain — checker, backends, LSP —
knows those methods belong to it. This arc builds that binding. It is a *surface and static
knowledge* arc: the runtime capability underneath (N3.4) is done and unchanged.

## Design

### The registry vocabulary: `ExtBundle`

An extension registers a named **method bundle** (in `noeta-native::registry`, alongside
`ExtModule`/`ExtType`):

```
ExtBundle {
    name:        &'static str,          // surface name: `impl <ext-root>.<name> for T`
    constraint:  PackedConstraint,      // what a binding type must look like (validated at impl site)
    methods:     &'static [BundleFn],   // the methods a bound type acquires
    ctx_dispatch: BundleCtxDispatch,    // one shared dispatch (both backends), receiver = slot 0
    ..DEFAULTS
}
```

- **`PackedConstraint`** is the static twin of the runtime `PackedView` check the kernels already
  do: field kinds in order (e.g. `&[F32, F32, F32]`), optionally a required layout (`Any` / `Row`
  / `Column`). Checked **at the impl site, at compile time** — the shape error moves from runtime
  dispatch to a diagnostic on the `impl` line.
- **`BundleFn`** is an `ExtFn` plus a **receiver kind**: `Element` (method on a value of the bound
  type: `v.dot(w)`, `v.length()`) or `Bulk` (method on `List<T>` of the bound type: `xs.dot_all(ys)`,
  `xs.sum()`, `xs.scale_all(k)`). Signature vocabulary is the existing `SigType`/`RetTy`, with
  `SameAsReceiver` semantics falling out of `SameAsArg(0)` (receiver rides as slot/arg 0, exactly
  the extern-type ctx-method convention).
- Uniqueness at install: duplicate qualified bundle name (`root + "." + name`) is a hard startup
  error, like modules.

### The binding: `impl vec.Kernels for Px {}`

The **standalone impl block** already exists (`impl Clone for Route {}`, attribute arc) and
`BuiltinTrait::from_name` is its resolution point. Bundles extend that resolution: when the trait
name doesn't match a `BuiltinTrait`, resolve it against the registry's bundles (qualified —
`vec.Kernels`; the parser must accept a dotted path in trait position, today it's a bare
identifier). Semantics at the impl site:

1. `Px` must be a `@packed` struct (a bundle is a packed-operations concept; E-code on violation).
2. `Px`'s declared fields must satisfy the bundle's `PackedConstraint` (kinds in order, layout)
   — else a compile-time diagnostic naming the expectation ("`vec.Kernels` requires exactly three
   `f32` fields; `Px` has `f32, f32, i64`").
3. The block must be empty (method bodies are native; mirrors the existing
   `standalone_impl_methods_unsupported` rule).
4. Conflicts: two bundles bound to the same type contributing the same method name = compile-time
   error at the second impl; a bundle method shadowed by a user-declared method of the same name =
   error (no silent override in either direction).

After binding: `Px` carries the bundle's `Element` methods; `List<Px>` carries its `Bulk` methods.
Both are ordinary method calls everywhere downstream.

### Checker

- Impl-site validation (steps 1–4 above; new diagnostics off the current free E-code).
- Method typing: the receiver-method lookup (`list_method` / user-type methods) gains a bundle arm —
  receiver `Named("Px")` or `List<Named("Px")>` where `Px` has bindings → the bundle's `BundleFn`
  sigs via the existing `sig_to_type_bound` machinery. `SameAsArg(0)` on a `Bulk` method types as
  the receiver's own `List<Px>`.
- Because binding is **nominal**, the argument-shape gap closes: `qs` in `xs.dot_all(qs)` types as
  `List<Px>` (or whatever the sig declares), not `Dyn`.

### Backends (one shared route, twice plumbed)

Method dispatch on an object/packed-list receiver, after user-declared methods and built-in
receiver methods miss: resolve the receiver's element **shape → bound bundles** and route to the
bundle's `ctx_dispatch` with the receiver as slot 0 — structurally identical to
`call_ctx_type_method` (extern-type ctx methods), so both backends are thin plumbing over existing
machinery and the differential holds by construction. Shape→bundle resolution is cacheable
per-call-site exactly like the H5 extern route cache; start uncached, gate on the bench.

The binding must be **runtime-visible**: lowering records `(shape, bundle)` pairs from the checked
program (the impl blocks) into the module, so dispatch consults program data, not global state —
a type bound in one module is bound exactly where the program says so.

### LSP

`members_of` + completion consult the bindings: a `Px` receiver offers the bundle's `Element`
methods, a `List<Px>` receiver its `Bulk` methods, with signature detail from the registry.
Hover/diagnostics come free via the checker. (Note: general registry-fed member completion for
*module* functions — `vec.` — is a sibling gap worth folding in here or doing alongside.)

### Dogfood + gates

`std.vec` registers `vec.Kernels` (constraint `[F32;3]`, any layout): `Element` = the scalar family
(`dot`, `length`, `normalize`, …), `Bulk` = the `*_all` family, dispatch delegating to the same
`vec3` kernels. Conformance: bound-type method calls on both layouts + boxed fallback + impl-site
error cases; differential + leak as always. **Perf gate:** `tests/bench/pm-native/` fixtures
rewritten in method form must match the module-function numbers (same dispatch depth ± the
route lookup). Third-party proof: the composed imgfx fixture binds the app's `Px` to a bundle.

Open surface decision (user): does the module-function form (`vec.dot_all(xs, ys)`) stay public
alongside methods, or become internal once bundles land? (Recommend: stays — it's the low-level
form and costs nothing.)

## Slices

- **K0** — `ExtBundle`/`PackedConstraint`/`BundleFn` vocabulary + registry lookup/install
  uniqueness (+ unit tests).
- **K1** — parser: dotted path in trait position; checker: impl-site resolution + validation
  diagnostics (packed-only, constraint, empty body, conflicts).
- **K2** — checker method typing (receiver → bundle methods, nominal argument types) + lowering
  the `(shape, bundle)` binding table into the module.
- **K3** — backend dispatch fallthrough (VM + eval, shared route), differential green.
- **K4** — LSP: members/completion from bindings (+ optionally module-fn member completion).
- **K5** — `vec.Kernels` dogfood + conformance + perf gate (method form ≡ module form).
- **K6** — third-party proof on the composed fixture + docs (`Native-Extensions.md` bundle
  section; `Standard-Library-Modules.md` vec surface).

## Follow-on (separate arc): `std.vec` as a hybrid package with canonical types

When first-party package distribution exists, evict vec/quat into a **hybrid package** shipping
canonical `@packed` types *as Noeta source* — full language citizenship, zero new machinery —
pre-bound to the bundles in the package's own source, next to the native kernels:

- **`Vec2`, `Vec3`, `Vec4`** — component-wise family per width (the flat-buffer kernels are
  already width-agnostic where element-wise; reductions key stride off `PackedView.byte_size`).
- **`Quat`** — **distinct type from `Vec4`, same 4×`f32` layout** (decision 2026-07-09): identical
  storage, disjoint semantics — `Vec4` gets the component-wise/dot/lerp family; `Quat` gets the
  Hamilton product (`mul`, non-commutative), `conjugate`, `slerp`, `rotate_vec3`, unit-`normalize`.
  Sharing one type invites exactly the bugs nominal typing exists to stop (component-wise
  quat "addition", `lerp`ing rotations); every serious math stack (glam, Unity, Unreal,
  DirectXMath) keeps them separate for this reason. The shared layout means the buffer kernels
  and future SIMD paths are common under the hood; only the bundles differ.

Users then `use vec.{Vec3}` and everything works out of the box; their own types still opt in
with one `impl` line.

### Type inventory (decision 2026-07-09: one module, organized together)

All spatial-math types live in **one package/namespace** (working name `vec`; real name — `geometry`?
`spatial`? — is an eviction-time decision, since the contents outgrow "vec"). Tiered:

**Tier 1 (the package's reason to exist):**
- `Vec2` / `Vec3` / `Vec4` — component-wise family, `dot`, `length`, `normalize`, `lerp`, per width.
- `Quat` — layout-twin of `Vec4`, disjoint bundle (Hamilton `mul`, `conjugate`, `slerp`,
  `rotate_vec3`, unit-`normalize`).
- `Mat3` / `Mat4` — the biggest gap in today's surface: 9/16 `f32` packed, `mul` (mat×mat,
  mat×vec), `transpose`, `inverse`, `determinant`, and the constructors that make 3D usable
  (`perspective`, `ortho`, `look_at`, `from_trs(pos, rot, scale)`). Carries the single most classic
  bulk kernel there is: **transform a `List<Vec3>` by one `Mat4`** (vertex transform) — a `Bulk`
  bundle method (`points.transform_all(m)`) and the flagship demo of the whole kernel machinery.
  (`Mat2` only if free; rarely used.)

**Tier 2 — a separate `geometry` module (decision 2026-07-09):** `Ray` (origin + dir), `Aabb`
(min/max `Vec3`), `Plane`, `Sphere`, `Rect` live in their own **`geometry` module**, consuming the
core linear-algebra types — not mixed into the vec/mat/quat module. They are compositional
`@packed` structs *of* the Tier-1 types (nested packed structs flatten inline, already supported)
whose methods (`intersects`, `contains`, `closest_point`, …) are scalar math with **no bulk loops →
plain Noeta methods in module source, zero native code**. Deliberately so: geometry demonstrates
that the Rust footprint is only the hot loops, everything else is language. Same package as the
core module or a sibling package depending on it — an eviction-time call (a sibling pure-Noeta
package is the cleaner layering, but any geometry consumer pulls the core's native crate
transitively anyway, so there's no toolchain saving either way). (Bulk forms —
`ray.hit_all(aabbs)` — can join a bundle later if profiled.)

**Deferred (with triggers):** integer vectors (`IVec2`/`IVec3` — grid/texel coords; trigger: demand),
`f64` vectors (trigger: scientific use), `Transform`/affine TRS type (Mat4 covers v1; trigger:
scene-graph work), `Color` (vec4-shaped but belongs to a color-space module, not here).
