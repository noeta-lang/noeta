# Forward design backlog — ideas captured outside any slice

This file holds **forward-looking design ideas** that are worth keeping but are not yet tied to a milestone slice. It is distinct from `deferred.md`: that file tracks work a specific slice deliberately punted (slice-deferrals, with a source slice + trigger); this file holds *new* design proposals that no slice has introduced yet, so they have a discoverable home until a planning pass picks them up. When an entry graduates into a slice, strike it here and point to the slice (same discipline as `deferred.md`).

## Typed JSON deserialization — `@derive(FromJson)` / type-directed parse

**Status:** proposal; **gated on the inferred-static type-system track** (see the `type-system-direction` memory + the "Type checker / inference hardening" block in `deferred.md`). Flagged as an **acceptance test** for that track — it cannot be implemented before it, and it exercises exactly the capabilities the track delivers.

**The idea.** A typed JSON-in direction symmetric with the existing `@derive(ToJson)`: from a record's field types, generate a fallible deserializer where *the type declaration is the parsing spec* — no schema, struct tags, or hand-written validators. Surface roughly `let u = from_json::<User>(payload)?` (or `User.from_json(payload)?`), returning `Result<User, JsonError>` whose error carries a path + reason (`.address.zip: expected string`). The generated deserializer type-checks each field against its declared type (`"age": "30"` → error, not a silent coercion), treats a missing `T` field as an error but a missing `Option<T>` field as `None` (required-vs-optional encoded in the type, not in annotations), checks ADT fields against the known variant set, and recurses into nested objects with the path propagating.

**Why the design is unusually well-positioned (verified against the codebase, 2026-06-22).** Several already-shipped decisions compound into the right foundation:
- **All-fields-literal choke point (architecture §4.1, enforced at compile time):** a record can only be constructed by assigning every field, so a deserializer literally cannot produce a partial object — the "missing field silently zero-filled" bug class is structurally impossible. This is real and shipped, not aspirational.
- **`Result`/`?` fallible-by-default:** the natural signature `Result<User, JsonError>` forces malformed input to be handled at the boundary (vs. TypeScript's `JSON.parse(): any`).
- **Fallible constructors (architecture §200):** associated functions returning `Result<Self, E>` / `Option<Self>` already exist, so the deserializer can optionally route through a validating constructor instead of the raw literal (see the open question below).
- **`@derive(ToJson)` already ships** the OUT direction (structural, in both backends) — `FromJson` is the symmetric partner.
- **Coherence with ORM hydration (architecture §9.12):** "hydrate a DB row into a typed object" and "parse JSON into a typed object" are the *same* operation — fill the all-fields literal, fallibly, from an external untyped source. Both could unify under one "structured deserialization" concept.

**Why it is gated (the codebase reality the proposal must respect).** `@derive(ToJson)` works today because it walks a *runtime* value's existing fields and needs **zero static type information**. `FromJson` is fundamentally different: producing a typed value *from* untyped input requires the target's **static field types** — to type-check each field, to distinguish `Option<T>` (absent → `None`) from `T` (absent → error), and to validate against a **static, closed** ADT variant set. Today the checker is `Unknown`-tolerant and generics are erased (`T` is `Unknown`), so none of that information is available at a deserialize site. Therefore `FromJson` is a **downstream consumer of the inferred-static type system**, not something buildable on the current substrate the way `ToJson` was. Conversely, it is an excellent end-to-end proof *for* that track: "the type is the schema" only holds if static field types, real `Option<T>` vs `T`, and closed-world variants are all genuinely present and non-erased.

**Open decisions to settle when this graduates:**
1. **Ring placement.** In-box **Ring 2** `@derive(FromJson)` (symmetric with the bundled Ring-2 `ToJson` and the Ring-2 `json` module), or part of the **Ring 3** first-party-but-not-bundled `Serialize`/`Deserialize` serde surface (architecture line 515, where generic de/serialization currently lives)? The doc today files generic `Deserialize` under Ring 3; making the *typed-object* path in-box is a deliberate change, not a default. Note: in code only `Serialize` is registered (as a no-op marker); there is no `Deserialize`/`FromJson` trait yet.
2. **Shape-only vs. integrated validation.** Default the derived deserializer to **shape-only** (right fields, right types, predictable, zero-config) with **semantic validation as an explicit opt-in** — either a separate `validate()` step or a `@derive(FromJson, via: new)`-style hook routing through a fallible validating constructor. Lean shape-only-by-default with validation layered (consistent with the "credible version first, powerful version layered" discipline), but decide deliberately.

**Provenance:** proposed in a separate design conversation (no codebase access); reviewed and corrected against the code + `architecture.md` on 2026-06-22 (ring conflation fixed; the type-system gating added).

## Bit-level computation arc — bitwise ops → fixed-width integers → packed types & SIMD

**Status:** full design plan written at **`plans/bitwise/README.md`** (not started). Provenance: arose
from a user question (2026-06-25) — "do we support bitwise operators and masks, like the Zed rope
optimizations?" Answer: **no support of any kind today** (no `& | ^ << >>`, no complement, no unsigned
or fixed-width ints, no popcount/SIMD; the only integer type is signed i64).

**The arc, in three independently-shippable tiers** (detail + slices + decision points in the README):
1. **Tier B — bitwise/shift operators on `int`** (signed i64): `& | ^ <<`/`>>`, complement via `!`
   (Rust-style, avoids the `~`-is-concat clash), hex/bin/octal literals + `_` separators, and the
   popcount-class intrinsics (`count_ones`/`leading_zeros`/…). **Small and self-contained** — operators
   are new `Op::Binary` discriminants resolved in the shared `apply_binary`, so both backends agree for
   free; no value-repr change. High value (unblocks all flag/mask work). Two real hazards flagged: the
   **`>>` vs nested-generics** lex clash (don't lex `>>` as one token — compose it in the expression
   parser), and reusing the existing `Pipe` token for expression-position `|`.
2. **Tier W — fixed-width integers** (`u8/u32/u64`, …): the layer that makes masks *correct* (defined
   wraparound, logical zero-fill shift, no sign-extension, exact-width popcount). A real type-lattice +
   checker + value-repr expansion — **the type-system track explicitly "gates packed-types/SIMD."**
   Recommended repr: **erase-to-i64 + type-directed masking** (union-erasure philosophy — width in the
   type, a shared mask helper in both backends, no new NaN-box tags). Four decision points to settle
   with the user first (which types, subtyping, repr, overflow policy → recommend wrapping-by-default).
3. **Tier P — packed types & SIMD** (`Simd<T, N>`): the Zed-blog class proper (SIMD scan, lane
   reductions, **`movemask` → `trailing_zeros` = mask→index**). Milestone-scale; **prerequisite: const
   generics** (`<const N: int>`, which the S-track's bounded *type* generics do not yet provide). Key
   oracle move: **scalar fallback semantics first** (portable, both backends agree by construction);
   real SIMD codegen is a later **perf-only** swap behind byte-identical semantics, gated on a bench.

**Suggested sequencing:** Tier B in full is the cheap high-value start and may be all that's needed.
Tier W only when *correct* unsigned masks are required (settle its decisions first). Tier P only when
SIMD throughput is the goal and the const-generic prerequisite is resolved. Optional capstone: a `Rope`
stdlib type (chunk-presence `u64` summary + SIMD newline scan) proving every primitive composes.

**Next free diagnostic code at time of writing:** E0034 (E0033 went to Phase-5.2 mut-fields; provisional allocations E0034–E0039 in the
README). When Tier B graduates into slices, strike this entry and point to them.

## Object-model redesign — `struct`/`class`/`enum`/tuple + dev-tier blocks

**Status:** design, not scheduled. Full doc at **`plans/object-model-redesign/README.md`**. From a
2026-06 design discussion (it replaced an earlier `resource`-kind proposal — a resource is just a class
with a `destruct`). Makes the kind keyword the
value/reference distinction: **value `struct`** (rename of `record`; COW; packed when all-primitive)
vs **reference `class`** (identity, sharing, `!Send`, and `destruct` — so a "resource" is just a class
with a destructor; file handles/connections are classes). Methods + bodies on all three kinds (enums
gain a body); `==` structural for struct/enum, identity-default + `Equatable` for class with `===`
always identity; opt-in per-field defaults; **tuples** for throwaway heterogeneous grouping (no
anonymous structs); standalone `impl` becomes uniformly optional. Plus a **dev-tier blocks** slice:
co-located, tree-shaken (via the existing DCE), manifest-discovered `test`/`bench`/`doc` blocks built
on one `@dev`-declared-tier primitive (content-kind = code|text), `test` implemented first — the clean
co-located-TDD experience PHP can't strip. Re-scopes Phase 5.2 (class-as-value → struct), breaking
surface migration (`record`/`type X={}` → `struct X{}`).

## Vulnerability / advisory intake — beyond operator-curated

**Status:** parked (2026-07-15), from the namespace-protection arc. The **advisory feed** ships (registry `src/advisory.ts` + migration `0007`/`0008`; client `noeta-pm/src/advisory.rs`; `noeta audit` matching + exit code; transparency-log-bound issuance). But there is exactly **one intake path today: operator-curated.** `POST /v1/advisories` is **admin-only** (`ADMIN_TOKEN` bearer + `ADVISORY_PRIVATE_KEY` to sign), idempotent per `id` (re-POST updates/withdraws, appends a new log leaf). No one but the registry operator can register a vulnerability.

**The gap — intake paths worth building (each independent):**
1. **Self-service, scope-owned advisories.** Let a scope owner file an advisory against *their own* `company/*` packages, authenticated with the scope's publish token (the same owner check `set_scope_policy` already does). Turns "operator files everything" into "maintainers disclose their own." Signing stays registry-side (one advisory key) or moves to per-scope keys (bigger change; the scope already has a provenance key).
2. **A reporting / triage queue.** A public `POST /v1/advisories/reports` (unauthenticated or lightly rate-limited) that lands a *candidate* in a pending state; an operator/maintainer promotes it to a signed advisory. Separates "anyone can report" from "only trusted parties publish."
3. **Upstream import (OSV / GHSA / CVE / RUSTSEC).** A sync job mapping external advisory IDs → `company/package` + affected `ranges`, re-signed and logged locally. This is how the feed gets real coverage without hand-authoring; needs a name-mapping step (external ecosystem coords → Noeta scope/package).
4. **A transparency-log monitor** (the anti-*suppression* complement, already flagged in the arc): a standing service that enumerates **all** advisory leaves in the log and cross-checks a served feed omits none. Per-advisory inclusion is verified client-side today; full suppression detection is this separate service.

**When it graduates:** decide the trust model first — who may *publish* (operator only, scope owners, or a promote-from-report queue) vs. who may *report* — because that choice drives the auth surface and whether per-scope advisory signing is needed. Reserved-namespace + owner checks from the arc carry over. Strike this entry and point to the slice(s) when picked up.

## Packed-field-kind enum duplication (low priority — mostly inherent)

**Status:** noted, not scheduled. Four parallel enums encode "what kind is each packed field":
`lang_ast::reflect::PackedKind` (`Struct(Box<PackedLayout>)`, check-time channel), `lang_bytecode::
PackedFieldDef` (`Struct(u32)` shape index, serialized), `lang_object::PackedKind` (`Struct(Rc<
PackedSchema>)`, VM runtime), eval `SlotKind` (`Struct(Rc<PackedSchema>)`, tree-walker runtime). The
four primitive leaves (Int/Float/F32/Bool) are identical, but the `Struct` variant *must* differ per
phase (a portable layout before shapes exist; a u32 index in non-`Rc` bytecode; a resolved `Rc<schema>`
at runtime) — standard phase-appropriate re-encoding, not copy-paste. Only the leaves are truly
shareable, and hoisting them into a common `PrimKind` would couple four crates to a shared crate for a
4-variant enum (likely costs more than it saves). **Natural time to revisit:** when `vec`/`quat` leave
core for a package and the native-extension API must expose packed layout *across* the package boundary
— a single public layout vocabulary would then earn its keep and could subsume some of these. Until
then: leave it. (The native-extension registry deliberately does **not** add a 5th copy — its bulk
packed kernels stay per-backend; see `plans/native-extensions/README.md` option B.)
