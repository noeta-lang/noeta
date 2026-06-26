# Object-model redesign — `struct`/`class`/`enum`/tuple on one axis, + dev-tier blocks

**Status: design, NOT scheduled.** A consolidated proposal from a design discussion (2026-06). It
**replaces an earlier `resource`-kind proposal** (removed) — the `resource` kind dissolves into "a class
with a `destruct`". It re-scopes parts of the completed memory-management **Phase 5.2** (which made classes
value-semantic) and is independent of the **inferred-static type-system** track (that's about the
*checker*; this is about the *object/value model* and surface). Pre-implementation: this captures the
decisions, rationale, open questions, migration impact, and a sane slice order — not committed work.

## Why this exists

Introducing a reference type for I/O handles (the `resource` idea) exposed a redundancy: once `class`
is value-semantic (Phase 5.2), **value-vs-reference is no longer what separates `class` from `record`**
— both are values, so the distinction collapses to a bundle of feature-gates (methods, `mut`, a
destructor, encapsulation). `class` ends up squeezed: if value, it's a record-with-features; if
reference, it's the resource. The resolution is to make **the kind keyword itself the value/reference
distinction**, the way Swift/C#/Java/Kotlin do, and let everything else (methods, `mut`, encapsulation)
be features available on both.

## The model: one axis

| | `struct` | `class` | `enum` | tuple |
|---|---|---|---|---|
| Semantics | **value** (COW; packed when all-primitive) | **reference** (identity, sharing) | value (sum type) | value (anonymous, positional) |
| `mut` fields | yes — in-place on packed, COW-when-unique on boxed | yes — in-place on the shared instance | n/a (variants) | no (positional value) |
| Methods | inherent + traits | inherent + traits | inherent + traits | — |
| `==` | structural | **identity** default, structural via `Equatable` | structural | structural |
| `===` | — (no identity) | identity, **always** (even with `Equatable`) | — | — |
| `destruct` | **no** (pure data) | **yes** (this is "a resource") | no | no |
| `!Send` across isolates | no (copied) | **yes** (reference) | no | no |
| Use it when | you have **data** | you need **identity / sharing / a lifecycle** | a closed set of cases | a throwaway heterogeneous grouping |

The single question a user faces is **"do I need identity, sharing, or a lifecycle (close/cleanup)?"** →
`class`; otherwise `struct`. That's the whole axis. "Data vs behavior" is *not* an axis (both have
methods); "mutable vs immutable" is *not* an axis (both can have `mut`, and both are immutable by
default). This is the clarity the old `record`/`class` split obscured.

### Naming: `record` → `struct`

"record" connotes a database row / filing entry — wrong for an in-memory value. `struct` says
"a structured value" with no baggage. Rename.

### `struct` — the value kind

- Value semantics, copy-on-write. **Packed** (unboxed, contiguous, no shape header) when all fields are
  primitive (or other packed structs) — the SIMD-amenable numeric path; a packed struct is copied by
  value and mutated in place with **zero** COW/refcount cost (the `points[i].x = …` case). A struct
  with a heap field (a `List`, a `class` reference, …) is boxed and uses the Phase-5.2 COW
  mutate-when-unique path. **`mut` does not prevent packing** — a field can't change type or size, so
  the layout is stable.
- **Methods** (inherent `fn` in the body, and in-body `impl Trait { }`), `mut` fields, structural `==`,
  no `===` (values have no identity).
- **No `destruct`** — pure data needs no cleanup; it's GC-managed. A struct may *hold* a resource (a
  `class`) by reference; the resource cleans itself up when its own refcount drops.

### `class` — the reference kind

- Reference semantics: aliasing shares the instance, `mut`-field mutation is visible through every
  alias (mutated in place — *simpler and cheaper* than the value path: no COW, no uniqueness check).
- **Immutable-by-default still holds**, which is what keeps sharing safe: a class with no `mut` fields
  is a shared *immutable* object (free to alias, like Clojure structures); only `mut` fields create
  genuinely shared *mutable* state, and those are explicit. The refined invariant: *struct-vs-class is
  value-vs-reference; `mut` is the only road to shared mutation, always opt-in.*
- **`!Send` across isolates** (the constraint that was going to define `resource`). The architecture's
  "no shared mutable objects" guarantee was always cross-isolate; within an isolate, shared mutable
  classes are normal. Communication across isolates is by copies (structs) or immutable handles.
- **`destruct` lives here.** A class *with* a destructor is what we'd informally call a resource
  (file handle, DB connection, lock, transaction) — close-on-last-release. A class *without* one is a
  plain shared mutable object (a graph node, a cache). The test "does this need a `destruct`?" is
  exactly "is this a resource?" — which is why lifecycle is the clean place to draw the line.
- **Classes can form reference cycles** (`a.next = b; b.next = a` ties a knot — reference mutation
  doesn't copy). This is what makes the memory-management **Phase 6 cycle collector** load-bearing
  (value structs alone can't cycle). Destructor-on-collect already runs a reclaimed class's `destruct`.

### `enum` — gets a body

Enums gain a body (methods + in-body `impl Trait { }`), matching struct/class:
`enum Status { Pending, Paid, Refunded; fn label(): string { match self { … } } }`. This makes all
three kinds structurally identical — `keyword Name { members; fn …; impl Trait { … } }` — and makes the
standalone `impl` form uniformly *optional* (today it's the only way to give an enum a trait, because
`EnumDecl` has no method body).

### Tuples — the anonymous, positional value

A tuple `(int, string)` is the anonymous positional counterpart of a struct: heterogeneous,
fixed-arity, value-semantic, structural equality, packable when all-primitive (`(float, float)` lays out
like a `Vec2`). For the **throwaway** case a named struct is too heavy for:

- multiple return — `fn divmod(a: int, b: int): (int, int)`,
- inline pairing — `xs |> map(fn(x) => (x, x * x))`,
- destructuring — `(q, r) = divmod(10, 3)`, and tuple patterns in `match`,
- positional access — `.0` / `.1` for the rare non-destructured case.

`()` is the existing `unit` (0-tuple falls out). Guidance baked into the design: **tuples for
throwaway/positional; a named `struct` once fields deserve names or there are more than ~3** (Rust's
stance — `(x, y)` fine, `.4` a smell). This is why we **do not add anonymous structs**: a map can't
hold them (a map is homogeneous/dynamic; an anonymous struct is heterogeneous/static), but in a
*nominal* language they'd be the one structural exception, dragging in structural-compatibility rules
for marginal benefit. Named structs cover ad-hoc grouping; tuples cover throwaway heterogeneity.

## Equality model (`==` / `===`)

- **`struct`/`enum`/tuple:** `==` is structural (value equality); there is no `===` (no identity to ask
  about).
- **`class`:** `==` defaults to **identity**; implementing `Equatable` makes `==` your custom
  (structural) comparison. **`===` is always identity** — *same instance* — independent of `Equatable`.
  Implementing `Equatable` changes `==` only, never `===`. So two distinct instances with equal fields:
  `a == b` true (your `Equatable`), `a === b` false — both useful, distinct questions.
- A class **without** `Equatable` may still use `==` (it falls back to identity) — friendlier than
  requiring the conformance to compare at all.

## Field defaults — opt-in, uniform

Add per-field defaults (`x: float = 0.0`), allowed on **both** kinds (it's a construction concern,
orthogonal to value/reference). They compose with the **full-initialization guarantee**: a default is an
*explicit declared value*, not a silent zero/null, so the object is still fully initialized — the
literal requirement just relaxes to "every field *without* a default must be set." Keep them
**opt-in per field**: a field with no default stays mandatory (preserving the "adding a field forces
every call site to reconsider" discipline), and a default is a deliberate "this field is genuinely
optional" signal. More useful for structs (public literal: `Point { x: 5 }`) than classes
(constructor-mediated), but not kind-exclusive. Independent of the taxonomy; ship separately.

## Traits, methods, coherence — mostly unchanged

- **Inherent methods** in the body (`fn foo(self)`) on all three kinds — don't force behavior through
  the trait loophole (`impl OneMethodTrait for T {}` to fake a method). Traits stay for *shared
  interfaces across types*.
- **In-body `impl Trait { }`** and the **standalone `impl Trait for T { }`** (same-module orphan rule
  E0013, uniqueness E0027) both remain; once enums have bodies, the standalone form is uniformly
  *optional* (an organizational choice — keep a lean data declaration and add behavior elsewhere).

## Removed / changed surface

- **`record` keyword → `struct`.** Unified body grammar: `struct`/`class`/`enum` all `keyword Name { … }`
  with newline-separated `name: type` fields (no commas/semicolons), methods, and in-body impls.
- **`type X = { … }` map-literal declaration retired.** It conflated "structural type alias" with a
  nominal declaration and can't hold methods. `@attribute type Foo` → `@attribute struct Foo`. If an
  inline *anonymous structural* type is ever wanted, that's a separate feature and the `{ … }` syntax
  would belong to *it*, not to named declarations (today it's the map literal — left unambiguous).
- **No anonymous structs** (tuples cover the use case).
- **Nested namespaces NOT added.** One `namespace App.Orders;` per file (C#/PHP model), dotted
  hierarchy, `use` imports. The motivation people reach to nested modules for — co-located tests — is
  handled by dev-tier blocks below, *without* opening "multiple namespaces per file."
- **No general-purpose pointers/references** (carried over from the dropped `resource` proposal, still
  the conclusion). COW removes the performance motivation (passing is a refcount bump); identity and
  shared-mutable-state are exactly what `class` provides, named; "mutate through a function" is
  value-return (`x = f(x)`, O(1) under COW) with an optional `inout` *sugar* later (value-semantic, not a
  pointer); graphs/cycles use arena indices or reference classes. General references would reintroduce
  the aliasing the language eliminates — `class` *is* the controlled re-introduction. Pointers stay an
  internal compiler/runtime concept only.

---

# Dev-tier blocks (`test` / `bench` / `doc`, on one primitive)

A co-located, tree-shaken developer-tooling experience — natural TDD, and clean strip-from-production
that PHP fundamentally can't do (a PHP test method stays a real class member). The reusable thing is the
**infrastructure**, exposed to external tools; the surfaces are content-appropriate.

## The primitive (general, exposed to consumers)

A **dev-tier block** is co-located content that is:
1. **Tree-shaken out of production** — a non-root for the existing DCE/capability discipline (§9.8.1,
   the same eliminator that strips unused reflection metadata). Zero production cost, *guaranteed by the
   build tier*, not by convention.
2. **Discovered via the build manifest** — the same manifest behind `attributes_of` / `roles_of`; a
   runner enumerates blocks by tier exactly as it enumerates attributes today.
3. **Same-namespace access** when in-source (sees private internals — test what you don't export); a
   block in a *separate* file is a different namespace and sees only `pub` (the unit-vs-integration
   split falls out of *location*, like Rust).

This trio is already general and content-agnostic — it's the reuse surface. **Exposing it externally** =
a tool registers a tier and reads its blocks from the manifest; no per-tier language change.

## Tiers carry a content-kind

A tier is a *name* + a **content-kind**, which is what lets `doc` (prose) and `test`/`bench` (code) be
one mechanism rather than special cases:

| tier | content-kind | block holds | consumed by |
|---|---|---|---|
| `test` | code | `fn`s (typechecked) | test runner — run + assert |
| `bench` | code | `fn`s (typechecked) | bench runner — measure |
| `doc` | text | markdown/prose (not typechecked) | doc generator — extract |
| `example` (later) | code | `fn`s, also rendered into docs | both |

The content-kind tells the parser how to read the body (parse-and-typecheck items vs capture text); the
tier *name* routes the body to the right tool; the run/measure/extract **semantics live in the external
tool**, not the language.

## Tiers are declared with a `@dev` directive

Consistent with `@attribute` (marks a record usable as an attribute) and `@semantic` (marks an enum
role-eligible): **`@dev` declares a tier** — its name, content-kind, production-excluded,
manifest-surfaced. Consequences:

- **Built-ins aren't special-cased.** `test`/`bench`/`doc` are `@dev`-declared tiers *in the prelude*;
  a library/tool declares a custom tier (`fuzz`, `snapshot`, `property`) with its own `@dev`. **All
  tiers — built-in and third-party — are used identically as `@<tier> { … }` directive blocks** (see
  "Block syntax" below); there is no special bare-keyword form.
- **Validation** — a block against an undeclared tier is a compile error (a typo'd `tset {}` doesn't
  silently vanish).
- **Uniformity** — built-in and third-party tiers are the same construct; "expose to consumers" is just
  "a tier is a declarable, manifest-surfaced thing."

## Extension-registered tiers feel first-party *for free*

The payoff of declaring tiers in-language (rather than via a separate plugin protocol): a third-party
tool registers a tier by **shipping a library that contains its `@dev` declaration** (+ a runner). A
project that depends on that library has the tier — and it integrates with the toolchain *automatically*,
because of an architecture commitment already in place:

> **The LSP is the same memoized query graph as the compiler** (architecture §9.6 — "the same query
> graph that powers a responsive LSP"). So an `@dev` tier declaration, being ordinary in-language code
> the *compiler* understands, is understood by the *LSP* with no extra work: completion, highlighting,
> diagnostics ("unknown tier", "this block's `fn` doesn't typecheck") all fall out. No LSP plugin API.

So the full extension story needs **no new mechanism**: a `bench` (or `snapshot`, `property`, …) tool is
just *a library with a `@dev bench` declaration + a manifest-reading runner*. The compiler validates and
type-checks its blocks, the LSP makes them feel native, the runner finds them via the same manifest as
`attributes_of`. That's the "feels first-party" experience without bespoke integration.

## Block syntax — `@<tier> { … }` directive blocks (uniform, conflict-free)

Blocks use the **`@`-directive syntax** — `@test { … }`, `@bench { … }`, `@doc { … }` — *not* a bare
keyword (`test { … }`). This is the cleaner design for three reasons:

1. **No identifier conflict, so no two-tier system.** `@bench` lives in the *directive* namespace, not
   the identifier namespace, so it can never clash with a variable named `bench`. That removes the
   reserved-keyword problem entirely — an **extension tier (`@bench`) is registered and used exactly
   like a built-in (`@test`)**, no bare-keyword privilege, no "general tagged form" fallback. One
   uniform syntax for every tier.
2. **Honest signaling.** The `@` says "this is a *compile-time directive* the build treats specially"
   (tree-shaken, tool-routed) — distinguishing a `@test { }` block from an ordinary statement block at a
   glance, the same way `@derive`/`@attribute` already flag compiler directives.
3. **It fits the existing family + Rust precedent.** `@derive`, `@attribute`, `@role`, `@semantic` are
   already `@`-directives; `@test`/`@bench` join them. (Rust likewise marks tests with an attribute,
   `#[test]`, not a bare keyword.)

**The one cost: directives must be extended to a *block* form.** Today a directive *annotates a
declaration* (`@derive(Comparable) struct X`). A tier block is a *standalone* directive carrying a body
(`@test { fns }`). The parser distinguishes by lookahead after `@name` (and optional `@name(args)`): a
`{` opens a block; a declaration keyword is an annotation. Bonus: the argument form gives **per-block
configuration for free** — `@bench(samples: 100) { … }`, `@test(skip) { … }` — which a bare keyword
couldn't carry. So `@dev test` *declares* the tier; `@test { … }` *uses* it (parallel to `@attribute
struct Foo` declaring an attribute and `#[Foo]` using it).

So: open-ended, LSP-native, conflict-free tiers — built-in and extension identical — all
`@<tier> { … }`, no bare-keyword exception.

### Annotation form too — `@<tier> fn` (code tiers)

The block is grouping sugar; the *base* form is the directive in its normal annotation position on a
single declaration — so a user doing true per-function co-location need not wrap each one:

```
@test fn adds() { assert(add(1, 2) == 3); }     // one test, no wrapping
@test { fn subs() { … } fn muls() { … } }       // block = "apply @test to every fn inside"
```

Both fall out of the one directive grammar (`@name` → a declaration is an annotation, a `{` is a
block); this is exactly Rust's `#[test] fn` + `mod tests`. The annotation form is for **code-content**
tiers (`@test`/`@bench`/`@fuzz` mark a `fn`); **text tiers (`@doc`) stay block-only** — you can't
annotate a function *with* prose (attached docs are doc-comments; `@doc { … }` is for standalone doc
sections).

## Discovery & opt-in via the package manifest (forward-looking)

`@dev` is the *compiler-level* primitive (defines a tier's name + content-kind so blocks parse and
typecheck). On top of it, the **package manifest** is the *distribution + activation* layer — the
backbone for the (yet-to-be-built) package registry:

- A **provider package declares the directives it provides** in its manifest (the stdlib provides
  `test`/`doc`; a bench package provides `bench`).
- A **consumer opts in via a *map*** in its own manifest — `local_name → providing_package` — uniformly
  for built-in (stdlib-provided) and third-party tiers. The map does triple duty:
  - **opt-in** (an entry activates `@<local_name>`; no entry ⇒ inactive),
  - **conflict resolution** — if two dependencies both provide `@bench`, the entry
    `bench = "criterion-lang"` picks one; *without* a disambiguating entry the compiler errors
    ("ambiguous `@bench`, provided by X and Y — choose in your manifest"), never a silent last-wins,
  - **aliasing** — to use *both* providers, map them to distinct local names
    (`bench = "criterion-lang"`, `microbench = "other-pkg"`), like import aliasing.
  The compiler/LSP resolve `@<name>` through the map to the bound package's `@dev` declaration; the
  version comes from the dependency pin, so the map is just `name → package`.
- **`lang init` pre-fills the common built-ins** (`test`, `doc`) so they work out of the box, **and
  they're removable** — nothing is forced. A minimalist project opts into nothing and has zero dev
  directives.
- The compiler/LSP **validate `@<tier>` against the opted-in set**: an unrecognized tier is a clear
  "not enabled — add it to your manifest" error, not a silent no-op. The manifest is the discovery
  registry tooling reads, and ties each directive to the package + version that provides it
  (provenance — you know what code processes your source).

This couples to the package system, so it's the **intended end-state, not a blocker**: the `@dev`
primitive (with the built-in tiers active by default) can ship pre-manifest; the
declare-and-opt-in activation lands with the registry.

## Block ergonomics

- **Multiple blocks per file** — drop a `@test { }` right under each function it exercises; the runner
  flattens every block into one set (test-fn names unique per file, which the checker already enforces).
  Optional labeled block (`test "arithmetic" { … }`) purely for runner output grouping.
- **Unit vs integration** — in-source `@test {}` (private access) vs a separate test-tier file
  (`pub`-only). One construct, two locations.

## Sequencing

Design the **primitive** general from the start (DCE gate + manifest surfacing + `@dev` tiers +
content-kind), but **implement `test` first** end-to-end (prelude declares the `test` tier, keyword
sugar, the test runner, private access, tree-shaking). Then `bench`, `doc`, and external tiers land as
*declarations*, not new language features.

---

## Migration & impact

- **Replaces the earlier `resource`-kind proposal** (removed) — `resource` becomes "a class with a
  `destruct`"; its still-relevant bits (`!Send`/isolate, must-close) are folded into the class section.
- **Re-scopes memory-management Phase 5.2.** Class-as-value (5.2a) moves to **`struct`** (the value kind
  keeps the `SetField`/COW mutate-when-unique machinery — *not wasted, relocated*); **classes become
  reference** with simpler always-in-place mutation. The 5.2b FileHandle decision ("reference") becomes
  the natural "FileHandle is a class." Phase 6's cycle collector gains its real justification (reference
  classes can cycle).
- **Breaking surface changes:** `record`/`type X = {…}` → `struct X {…}`; `@attribute type` →
  `@attribute struct`; class-`==` semantics (identity vs the current structural-for-objects). A migration
  pass, plannable since this is pre-implementation.
- Distinct from the **inferred-static type-system** track (the checker) and the **type-system-direction**
  memory — this is the object/value *model*, not type *inference*.

## Open questions — SETTLED (2026-06-26 design session)

1. **Generics** — **full reification (C#-style)** is the committed end-state, but as its **own
   milestone-scale track AFTER** this taxonomy arc; slices 1–6 run under today's **erasure**. Identity
   (`===`) is per-allocation, independent of type args, so nothing in the arc depends on the generics
   model. Monomorphization rejected (wrong shape for the IR-interpreter + register-VM).
2. **Visibility** — `struct` fields default **public** (transparent data; literal-constructible
   anywhere); `class` fields default **private** with per-field `pub` opt-in. **Parsed in slice 1**
   (optional `pub` on a field); **enforced in slice 2** (private-default + literal-gating). A struct
   field of `class` type still forces the struct boxed (confirmed). Construction stays the
   visibility-gated literal + an associated `fn` (the existing `new`-by-convention method; no new syntax).
3. **`mut` rule (asymmetric)** — `struct` `x.f = v` needs the field `mut` **and** a `mut x` binding (it's
   COW sugar for `x = {…x, f:v}`, a rebind); `class` `x.f = v` needs **only** the field `mut` (mutates
   the shared instance). The struct side already falls out of the binding-reassignment analysis in
   slice 1; the class relaxation is slice 2.
4. **Spellings** — the declaration directive is **`@tier`** (not `@dev`); the primitive is
   `name + content-kind + inclusion-rule`, generalizing to build-profile tiers (e.g. a `@debug` tier).
   Tuple access is **`.0`/`.1`**.
5. **Enum-body** — lands as **slice 3** (part of this arc).

## Suggested slicing

1. **Rename + unify declaration syntax** (`record`→`struct`, retire `type X={}`, one body grammar). Pure
   surface; large but mechanical. **✅ DONE (2026-06-26)** — `struct` keyword + unified body (fields with
   `pub`/`mut`, inherent methods, in-body `impl`, no `destruct`); `type X={}` retired (`type` reserved);
   internal `Record`→`Struct` rename across all crates incl. the reflection `Type.Struct` surface; struct
   methods wired end-to-end through the class-parallel path (IR `StructDef`, compiler protos/dispatch,
   both eval backends, checker method-body validation). Gates green: conformance 266/0, differential
   agrees (0-skipped), leak residency 0 both backends, clippy/fmt clean, miri clean.
2. **class = reference** (the semantic core): identity, `===`, `!Send`, in-place shared mutation; move
   the value-mutation machinery to `struct`; enforce field visibility (private-default classes). The
   load-bearing slice. **Sub-sliced for delivery (each a green commit):**
   - **2a — equality model. ✅ DONE (`55950d4`).** `===`/`!==` identity operators (never overload);
     class `==` defaults to identity unless `Equatable` (derived or hand-`impl`'d), carried by a
     per-type `structural_eq` bit on `Shape` (VM) + `TypeDef` (tree-walker), computed identically so
     both backends agree even on nested class fields. New **E0034** (`===` on a value operand). Next
     free **E0035**.
   - **2b — reference in-place mutation. ✅ DONE (`aee8d65`).** Class `x.f = v` mutates the shared
     instance (visible through every alias), no COW fork; struct keeps COW. Eval got `RefCell`
     interior mutability on `ObjectValue.fields` (a plain `Rc<…>` can't mutate a shared instance);
     the VM already writes slots through a raw pointer, so it skips the COW-copy branch for a class.
     `mut x` still required at this step.
   - **2b′ — asymmetric `mut` relaxation. ✅ DONE.** Class `x.f = v` needs only the field `mut`,
     **not** a `mut x` binding (it is in-place, not a rebind). Implemented via the **static checker**
     option (chosen with the user): the checker now tracks per-binding mutability (`VarBinding`) and
     rejects a **struct** field-set on an immutable receiver statically (kind-aware E0006); a `class`
     field-set is always allowed. Both backends then **skip the immutable-reassignment check for
     field-set binds** — a new `field_assign` flag on IR `Stmt::Bind` (set by lowering when the
     value is an `Expr::FieldSet`) routes the tree-walker through `assign_force` and tells the VM
     compiler not to emit the E0006 raise. So a class field can be assigned through an immutable
     binding or a **function parameter** (reference passing), while a struct still requires `mut`.
   - **2c — object-cycle collection. ✅ DONE.** Reference classes can finally form cycles
     (`a.next=b; b.next=a`), which precise refcounting cannot reclaim. Both backends needed exit-time
     work: the **VM** added a post-teardown backup `collect_trace(&[])` (its pre-teardown trace marks
     from the still-live globals, so a globals-rooted cycle was missed); the **tree-walker** had only
     a *closure-capture* reaper, so it gained a `MUTATED_OBJECTS` weak registry (a class cycle can
     only be *closed* by an in-place field assignment) + a two-pass `reap_object_cycles` (destruct
     all members with fields intact, then drain to break the cycle). **Cycle-finalization order is
     deterministic reverse-creation** (newest-first), chosen with the user as most consistent with
     the language's reverse-declaration teardown — both backends sort by a per-object creation `seq`
     (`ObjHeader::seq` / `ObjectValue::seq`) so cyclic `destruct` order agrees (the VM's live-object
     registry is a `HashSet`, otherwise arbitrary). Leak residency 0 on cyclic graphs, both backends;
     differential agrees on cyclic `destruct` output.
   - **2d — field-visibility enforcement. ✅ DONE.** `struct` fields are always public; `class`
     fields default private with per-field `pub` opt-in (parsed in slice 1, now enforced). New
     **E0035 PrivateField** on read `x.f`, write `x.f = v`, and literal construction `T { f: v }` of
     a private field from outside the declaring type. Privacy is **type-scoped** (chosen with the
     user): a private field is visible inside the type's own methods/destructor on `self` *and* any
     same-type value (so `Equatable`/`compare` can read `other.x`) — the checker tracks
     `current_type` + a `private_fields` registry. Migration (chosen with the user): externally-read
     class fields were marked `pub`; the `orders` demo's external `Order { … }` was routed through
     `Order.new(...)` (idiomatic constructor). Next free **E0036**.

**SLICE 2 (class = reference) IS COMPLETE** (2a equality model, 2b reference mutation, 2b′ asymmetric
`mut`, 2c cycle collection, 2d visibility; `!Send` deferred to the concurrency milestone).
   - **DEFERRED (confirmed with user, 2026-06-26): `!Send` across isolates.** Unenforceable today —
     no isolate boundary / value-transfer point exists in the codebase, so any check would be
     untested dead code. Revisit at the concurrency milestone (the slice that adds `spawn`/channels),
     which is where the boundary to reject a `!Send` class at will live.
3. **Enum bodies** (methods + in-body impls). Self-contained. **✅ DONE (`536680e`).** Enums gain the
   unified body, making all three kinds `keyword Name { members; fn; impl Trait { } }`. Parser:
   `enum` body = `choice(impl, method, variant)`, impl methods flattened into the dispatch table +
   block retained for the checker (as class/struct); `EnumDecl` gains `methods`/`impls`. An enum
   method takes the whole value as `self` (no implicit field scope — variants differ; the body
   `match`es on `self`, exhaustiveness-checked because the checker binds `self` to the enum type).
   Instance `v.m(...)` + associated `E.f(...)` resolve through the shared `(type, method)` table
   (variant construction still wins for a variant name); compiler reserves a proto per method (empty
   field scope), the VM dispatches an enum value through the same table as an object, the tree-walker
   resolves the `EnumDef` from scope. **Operator/protocol parity — UNIFORM, no per-kind limit
   (decided with user):** an enum's in-body `impl`s light up operators exactly as a class's, no
   allowlist / no new diagnostic — `Equatable`→`==`, `Comparable`→`< >`, `Display`→`${}`, even
   `Add`→`+`; both backends key on the value's shape name (one-line `is_object() || is_enum()`
   extensions). The checker records an enum's impl trait names so an operator trait is accepted on an
   enum operand. **Mem:** enum (and the symmetric, previously-missed *struct*) method bodies now go
   through the reuse + drop-insertion IR passes — a method-local allocation is otherwise unreclaimed
   on the IR backend. Conformance 277 / differential agrees / leak residency 0 both backends / miri
   clean. **No new diag code (next free still E0036).**
4. **Tuples** (value, positional, destructuring, patterns). **Sub-sliced: 4a DONE (`c43ba28`), 4b
   next.** 4a = the tuple value end-to-end — literal `(a, b, …)` (2+ elements; `(x)` is a
   parenthesized expr, `()` is `unit`, no 1-tuples), type `(int, string)` (element-wise-covariant
   `Type::Tuple`, multiple-return), positional projection `.N` (nested `x.0.1` works — the lexer
   takes `.0.1` as one float, the parser splits it into a chain), structural equality, display.
   Dedicated `Payload::Tuple`/`Value::Tuple` (value-semantic, owns one ref per element like a list,
   no shape); ops `MakeTuple`/`TupleIndex`; IR `Rvalue::Tuple`/`TupleIndex`; `as<(…)>`/`is (…)` narrow
   head-constructor only ("is a tuple"); `type_of` erases a tuple to `dyn`. No new diag code.
   Conformance 279 / differential agrees / leak 0 both backends / miri clean. **4b DONE
   (`fbe2978` + `f6c3dd8`) — tuple destructuring.** 4b.1 (`fbe2978`) = irrefutable: binding
   `(a, b) = expr` (new `Stmt::Destructure`) + `for (a, b, …) in …` (`ForPattern::Pair`→N-ary
   `ForPattern::Tuple`), both **desugared in IR lowering** to a temp/hidden-var + `.N` projections
   (reusing 4a's `TupleIndex`, so no new IR/VM construct); `.enumerate()` now yields `(int, T)`
   **tuples** (unified — the corpus only consumed it via `for (i,x)`); the list-based
   `DestructurePair` op retired. 4b.2 (`f6c3dd8`) = refutable: `match t { (p, q, …) => … }` via new
   `Op::MatchTuple` + 4a's `TupleIndex` extraction (mirrors variant patterns; nested patterns
   compose). No new diag code. **Slice 4 (tuples) is COMPLETE.** Conformance 283 / differential 275
   matched / leak 0 both backends.
5. **Field defaults** (opt-in per field). Independent. **✅ DONE.** Per-field defaults `x: T = expr`
   on `struct`/`class` fields, reusing the parameter-default thunk machinery: a field with a default
   is optional in a literal, filled at construction when omitted (full-init guarantee preserved). A
   default is evaluated in the **type's definition (global) scope** — resolves globals only, never
   `self`/sibling fields/the construction site (a sibling reference is `E0005`; a closed,
   self-contained choice mirroring parameter defaults). **No trailing-only rule** (literal fields are
   named, so a default makes its field optional regardless of position). Both backends fill from a
   shared `(type, field) → default-thunk` table (eval: `TypeDef.field_defaults` run via
   `run_field_default`; VM: `Module.field_defaults` protos run by `MakeStruct` via `run_thunk`,
   empty upvalues = global scope), so a missing field is filled identically — differential agrees,
   leak residency 0. Checker validates the default's type against the field type (`E0007`). No new
   diag code (next free still **E0036**). Conformance 284 / differential 276 matched, 0-skipped /
   miri clean. **DEFERRED (confirmed with user): the bare empty literal `T {}`** (a fully-defaulted
   type with no explicit field) — it re-collides with the `if cond {}` empty-block disambiguation,
   which the **slice-7** restricted-control-flow-head expression resolves; lands there. Until then a
   literal needs ≥1 explicit field or a spread.
6. **Dev-tier blocks** — the primitive (**`@tier`** tiers + content-kind + manifest) with **`test`
   first**, then `bench`/`doc`. **PLAN: `slice-6-dev-tiers.md`** (the full design, settled
   2026-06-26). The dev-tier prose *below in this README* predates two design sessions and is
   **superseded by that plan** where they disagree, on three points: (a) the directive is **`@tier`**,
   not `@dev`; (b) **tiers vs profiles are two axes** — a tier is `(name, content-kind)` (a source
   property), a **profile** (the build configuration, renamed from "target" to free that word for the
   platform triple) owns *inclusion*; (c) inclusion is a **provider-map** `tier → package` (not a
   list, not a separate opt-in map), doing activation + provenance + conflict-resolution-by-
   construction + aliasing in one place, and **text tiers (`doc`) go through it too** (not "always
   on"). Stripping is a **front-end filter before lowering** (no DCE pass exists / is needed). The
   profiles/TOML surface lands with the package manifest; slice 6 ships the in-language primitive
   against a resolved active-tier-set the CLI supplies. Env/compile-time-constants in profiles
   **deferred entirely**.
7. **Optional line-end semicolons** — a `;` at the end of a statement becomes optional; a **newline
   terminates a statement** (Go/Swift/Kotlin-style), with `;` kept valid (for multiple statements on one
   line) and inside `for (…;…;…)`-like positions. Pure surface, no semantics change. **Load-bearing
   decision to settle first: the line-continuation rule** — a newline must *not* terminate when the
   statement is syntactically incomplete: an open `(`/`[`/`{`, a trailing binary/assignment operator, a
   trailing `|>`/`??`/`.`/`,`, or a leading continuation on the next line. Pick the mechanism: (a) the
   **lexer** emits newline tokens and suppresses them after a continuation token / inside brackets
   (Go's approach — newline → synthetic `;`), or (b) the **parser** treats newline as a soft terminator.
   The unified struct/class/enum **body grammar already has no field terminator**, so bodies are
   unaffected; this targets statement sequences (`echo`/binding/`return`/`use`/expr-stmt). Last slice
   because it touches every statement production and wants the rest of the surface settled first.
   **⚠ Interaction with field-init shorthand (`b18f376`):** the struct/map literal punning
   (`Type { x }` ≡ `Type { x: x }`, `{ k }` ≡ `{ "k": k }`) made an object-literal body parse from a
   bare name. Today that is collision-free because a single-bare-name control-flow body
   (`if cond { x }`, no terminator) is *already* a parse error (statements require a terminator). Once
   a newline can terminate a statement, `if cond { x }` becomes a valid block and re-collides with
   `cond { x: x }` — so this slice's disambiguation **must** handle it: either a restricted
   control-flow-head expression (no top-level struct literal without parens, Rust-style — the
   recommended fix) or keep relying on the comma/colon disambiguator (which only covers multi-field /
   explicit-colon literals, not single-field shorthand).
