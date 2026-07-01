# P-SIMD follow-on — `@packed(layout: column)`: layout as a performance attribute

**Status: design, for sign-off.** Follow-on to [`p-simd.md`](p-simd.md) (P-SIMD is done — the opt-in
`vec.soa*` batch shipped, 2.7×–4× on `dot`/`length`). This doc proposes the *clean* home for that win:
a **layout parameter on the existing `@packed` directive**, so the columnar layout becomes a per-type
performance attribute that is **invisible to behaviour** — no new value type, no new method surface.

## The decision trail (how we got here)

1. Explicit SIMD (`wide`) benched *slower* than the autovectorized scalar loop, twice (AoS buffer, then
   SoA columns). The win is the **layout**, not intrinsics — contiguous same-type columns let LLVM
   vectorize across elements; the AoS stride-12 layout can't.
2. Re-laying-out the *general* packed list to columns would revert P-COW's O(1) append. So the win
   shipped as an **opt-in** thing (S4/S5: `vec.soa*` + a `SoaVec3` value type).
3. Design review (this doc): the opt-in should be a **directive on the type**, not a separate value
   type + parallel `soa_*` API. `@packed` is *already* a type-level layout directive ("store lists of
   this element as a flat buffer"); adding *which* flat layout is the natural, consistent extension.
4. And the ops should **fold into the normal list/`vec` API**, not live on a batch — so layout only
   ever changes *performance*, never the surface or the result.

## Surface

`@packed` gains one **named** argument, `layout`, with two values — chosen to read as *row-major vs
column-major* (spreadsheet/DB framing) and avoid the `AoS`/`SoA` jargon entirely:

```
@packed struct Vec3 { x: f32; y: f32; z: f32 }                 // ≡ @packed(layout: row)  (default)
@packed(layout: column) struct Particle { px: f32; py: f32; pz: f32; mass: f32 }
```

- **`layout: row`** (default) — store row-by-row: each element's fields are contiguous. Today's `@packed`.
  Bare `@packed` stays exactly this, so nothing breaks.
- **`layout: column`** — store column-by-column: each field's values are contiguous across all elements.

Think of a `List<T>` as a table: a **row** is one element, a **column** is one field. `row` stores the
table row-major; `column` stores it column-major. That's the whole idea.

## The invariant: layout is a pure performance attribute

> Changing a type's `layout` changes **only performance**, never any observable value. `RunResult` is
> identical; the differential's *0 skipped / backends agree* holds by construction (both backends read
> the same `schema.layout` and share the kernels).

Every element observed — `list[i]`, iteration, equality, display, `json.stringify`, `to_bytes` round-trip
— yields the same value either way. `to_bytes`' *byte order* differs between layouts, but the corpus only
observes `to_bytes` via **length / self-equality / `from_bytes` round-trip** (never the raw bytes; the
display is opaque `<N bytes>`), and `to_bytes`/`from_bytes` are mutually consistent per type — so even
serialization stays invisible. This is the same posture that let packed lists exist at all.

## No batch type, no new methods — ops fold into the normal API (in three tiers)

There is **no batch value type and no `soa_*` method surface**. Layout is storage only; operations are
the ordinary list/`vec` API. But the operations are *not* one uniform "fold into `vec.*_all`" — the
directive is **general** (any packed struct can be `column`) while the useful operations sit in three
tiers, only some of which are general:

1. **Generic per-column primitives** (stdlib; work on *any* `column` list because they are field-indexed,
   not type-specific): sum a field, field-wise `add`/`scale` two lists, map a field with a native op.
   Any numeric struct gets these for free — this is the general fast surface.
2. **Domain kernels** (`vec`/`quat` modules): `dot_all`, `length_all`, `cross`, quaternion mul. These are
   **shape-specific** — `dot`/`length` interpret three `f32`s as a vector, so they only accept a
   Vec3-shaped list (row *or* column) and simply run **faster** on `column`. They do **not** become
   general; the directive being general does not make `dot_all` general, and does not need to.
   ```
   @packed(layout: column) struct V3 { x: f32; y: f32; z: f32 }
   d = vec.dot_all(ps, qs)   // Vec3-only op; column is its fast path. Same call, same result.
   ```
3. **Type-specific kernels via the extension registry** — anything else (a `Particle` integrator, a
   user's own numeric struct). See [Kernels via extensions](#kernels-via-extensions-the-general-answer)
   — the general answer, gated on the native-extension ABI.

Per-element access (`ps[i]`) and append (`ps ~= [p]`) still work on a `column` list — they just pay a
gather / O(n)-rebuild cost (see the tradeoff table). It remains a real `List<T>`; only performance
changes. The user never writes `soa` anything.

## Kernels via extensions (the general answer)

Tiers 1–2 cover the type-agnostic field ops and the Vec3/quat domain. The *general* way an arbitrary
type gets a fast column kernel is to **register it in the native-extension registry**, keyed by
`(module/type, operation)` — the same seam that already registers `vec.add`/`json.parse`. Two rules:

- **Register the kernel; do not attach it to the directive.** `@packed(layout: column)` declares layout
  only. A package registers *many* ops for a type separately (`register_column_kernel(Particle,
  "advance", native_fn)`), keeping type-declaration and behaviour decoupled. Putting a kernel *in* the
  directive would bind one type-declaration to one function — strictly less flexible.
- **Blocker (why this is deferred, not built now):** the registry's neutral value seam (`NativeValue`)
  does not hand raw buffers to native functions — which is exactly why the bulk `vec.*_all` kernels are
  a **per-backend special case today** (they reach for `packed_vec3_data`'s raw bytes directly, outside
  the registry). Registering columnar kernels generically needs a new ABI capability: *"give me field
  `f`'s contiguous column buffer."* That is the **`lang-native` ABI extraction** the native-extensions
  track already defers to the package-manager milestone (`plans/native-extensions/README.md`).

So: **this arc ships tiers 1–2** (column layout + generic per-column primitives + the per-backend
Vec3/quat kernels); **tier 3 (third-party registered column kernels) is tracked as deferred** against the
`lang-native` ABI work — see `plans/native-extensions/README.md` and `plans/deferred.md`.

## Storage & implementation

**No new value type.** A column-layout list is just a `Payload::PackedList { schema, bytes }` (VM) /
`ListRepr::Packed(PackedList)` (tree-walker) whose **`schema` carries the layout** and whose `bytes` are
in column order. The layout is a type-level property, so it lives on the shared `PackedSchema`
(`lang-object`), which every op already threads:

```rust
// lang-object PackedSchema gains:
pub enum PackedLayout { Row, Column }
pub struct PackedSchema { shape, fields, byte_size, layout: PackedLayout }
```

**Column byte layout (leaf-flattened).** For `n` elements and leaf primitive fields `f0..fk` of widths
`w0..wk`, store columns end to end: `[f0×n][f1×n]…[fk×n]`. Field `f`'s column starts at
`base[f] = n·Σ_{j<f} w_j`; element `i`'s field `f` is at `base[f] + i·w_f`. (Nested `@packed` struct
fields flatten recursively to leaf columns, so `Vec3`'s `x/y/z` are three contiguous `f32` columns —
exactly the proven-fast case. **Slice 1 can restrict to structs of only primitive fields** — which
covers `Vec3` and the whole demonstrated win — and add nested flattening as a follow-on.)

**Ops that gain a `layout: column` path** (all mechanical, all bit-identical to the row path):

| op (`lang-value` / both backends) | row (today) | column |
|---|---|---|
| construct (`MakePackedList`, literals) | append rows | write column-order |
| `packed_get(i)` / `packed_field(i, f)` | slice one row | **gather** field(s) across columns |
| `packed_push` / `packed_extend` / `packed_concat` | O(1)/O(k) tail append | O(n) rebuild (insert per column) |
| `packed_select` (slice/filter/reverse) | copy row blocks | gather per column |
| `packed_set` / `packed_set_in_place` | write one row | write across columns |
| `packed_bytes` (`to_bytes`) | row-order buffer | column-order buffer |
| `vec.*_all` kernels | AoS `*_buffers` | columnar kernels (S4) |

**Kernels reused from S4.** `lang_stdlib::vec3`'s columnar reductions (`soa_dot`/`soa_length`, iterator-
zipped, autovectorized) are the fast path; they read the contiguous column byte-ranges directly (each
column is a contiguous `f32` run in the column-order buffer). No `wide`/intrinsics (they benched slower).

## S5 → directive migration

The shipped S5 surface is reworked into this cleaner shape:
- **Remove** the `vec.soa*` free functions, the `SoaVec3`/`SoaBatch` value type in both backends, the
  `Payload::SoaVec3` variant, the `SOA_VEC3` checker name, and `vec3_soa.lang`.
- **Keep** the S4 columnar *kernels* (`soa_dot`/`soa_length`/…) — they become the internal column
  kernels the `vec.*_all` dispatch calls. (They may be renamed `column_*` to drop the `soa` term.)
- The `vec3_soa.lang` behaviour is re-expressed as `vec3_column.lang`: same `vec.dot_all` etc., but on a
  `@packed(layout: column)` type, asserting identical output to the row-layout version.

Net: less surface than S5 (one directive arg vs eight `soa_*` functions + a value type), and the win is
reachable through the *existing* `vec` API.

## Tradeoffs (what `column` optimizes, and its cost)

| operation | `row` (default) | `column` |
|---|---|---|
| whole-collection field math (`dot_all`, `length_all`, sum/add/scale a field) | scalar-ish | **fast** (autovectorized columns) |
| read/build one element (`list[i]`, `list ~= [x]`) | **fast** (O(1) append) | slower (gather / O(n) insert) |

**Speed caveat (unchanged, and important):** the layout is the *precondition* for the win; the speed
comes through **native kernels**. A user-written per-element loop over a column-layout list still runs at
interpreter speed — it can't vectorize. `column` gives fast *native* whole-collection ops, not fast
*arbitrary user code*. (User-defined vectorized kernels are the separate `Simd<T,N>` / P-BITS Tier P
milestone, which needs const generics.)

## Oracle posture

- **Differential / leak / conformance:** unchanged gates. Layout is behaviour-invisible, so `0 skipped /
  backends agree` and residency 0 hold by construction (shared kernels, both backends read `schema.layout`).
- **`Send` / isolates:** a `layout: column` type classifies for `Send` exactly like its `row` form
  (value type, immutable columns) — no special case; it can cross isolate boundaries by copy like any
  packed list. (This is *stronger* than S5's `SoaVec3`, which was conservatively `!Send`.)
- **Bench-gated:** each op that gains a column path is benched row-vs-column; we keep the column path
  only where it wins (reductions) or is required for correctness (get/append/…), and record numbers here.

## Decisions (settled)

1. **Arg is an enum.** `layout` takes an enum value, variants `row | column` (an internal
   `PackedLayout { Row, Column }`), validated by the typed-directive-arg path (E0037
   `InvalidDirectiveArgument`) — `@packed(layout: colunm)` is a hard error, not a silent fallback. The
   parser's `@packed` arm (`lang-parser` ~2678, currently *rejects* all args) gains a small addition to
   accept the named enum arg. Future knobs (`aligned`?) slot in as more `@packed(...)` args.
2. **`dot_all`/`length_all` stay Vec3-specific** (tier 2, above) — accelerated by `column`, not made
   general. General column speed comes from tier-1 primitives and tier-3 registered kernels.
3. **S5 is hard-cut** (not aliased) — the `vec.soa*` surface only landed this session; nothing external
   depends on it. See the migration section.

## Open decisions (settle before coding)

1. **Leaf-flatten vs top-level columns:** recommend leaf-flatten (fully general — any numeric field
   contiguous), but **slice 1 restricts to primitive-only structs** (covers `Vec3`); nested flattening
   is a follow-on slice (C5).
2. **Which ops get a *fast* column kernel vs just a correct one:** reductions/field-math get fast native
   kernels; `get`/`push`/`concat`/`select`/`set` get *correct* column paths (not necessarily faster than
   row — column is not meant for per-element/append-heavy use). Bench decides if any need tuning.

## Slice plan

- **C1 — directive surface.** `@packed(layout: row|column)` parses (parser arm + AST carries
  `PackedLayout`), checker validates (E0037), `PackedSchema.layout` is populated. No storage change yet
  (column falls back to row internally) — pure front-end, conformance green.
- **C2 — column storage + correct ops. ✅ DONE.** `PackedSchema` carries a `column` flag (threaded
  reflect → bytecode `PackedSchemaDef` → both runtime schemas), populated from the directive via the
  checker's `column_structs` set. All packed ops gained a column path through one shared offset
  helper, `PackedSchema::field_offset(i, slot, count)` (row = `i·byte_size + prefix`; column =
  `count·prefix + i·width`): `packed_get`/`field`/`items` gather across columns, `push`/`concat`/
  `extend` rebuild (O(n), as designed), `select`/`set`/`set_in_place` scatter per column, `to_bytes`
  returns the column-order buffer (round-trip self-consistent). **Correctness, not yet speed:** the
  `vec.*_all` fast path (`packed_vec3_data`) *declines* a column list, so the kernels fall back to the
  element-wise scalar loop — correct on either layout (differential-pinned). The fast columnar SoA
  dispatch is C3. **Nested `@packed` fields work generally** (kept as contiguous per-element chunks,
  so no primitive-only restriction was needed); leaf-flattening them into leaf columns is still C5.
  Conformance 387 (+`packed_column_ops`), differential 377 / 0 skipped / agree, leak 0, miri clean.
- **C3 — fast `vec.*_all` column dispatch.** `add_all`/`sub_all`/`scale_all`/`dot_all`/`length_all` pick
  the S4 columnar kernels on `layout: column` (a column Vec3 buffer's bytes *are* the SoA columns).
  Bench row-vs-column, record here. Add `vec3_column.lang`.
- **C4 — retire S5.** Remove `vec.soa*` + `SoaVec3`/`Payload::SoaVec3` + `SOA_VEC3` + `vec3_soa.lang`.
- **C5 (follow-on) — nested leaf-flattening** for structs with nested `@packed` fields (`Particle`).

## Verification (every slice)
- Conformance green; `--differential` matched / **0 skipped** / backends agree; leak residency 0 both.
- `cargo test --workspace`, clippy, fmt clean. Standard commit trailers; no push without authorization.
- The slice's **bench numbers** (row vs column), recorded in this doc.
