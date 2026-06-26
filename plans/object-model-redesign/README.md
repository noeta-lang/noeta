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

- **Built-ins aren't special-cased.** `test`/`bench`/`doc` are `@dev`-declared tiers *in the prelude*,
  with keyword sugar (`test { … }` is sugar for a `test`-tier block). A library/tool declares a custom
  tier (`fuzz`, `snapshot`, `property`) with its own `@dev`.
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

**One deliberate limit — keyword sugar is a *language* privilege, not an extension capability.**
`test { }` works because the language *reserves* `test`. Letting an extension reserve a new bare keyword
(`bench { }`) would either break code using `bench` as an identifier or require contextual keywords whose
meaning flips on imports (parser-ambiguity risk). So:

- **Built-in / universal tiers** (`test`, `doc`, and `bench` if the language ships a runner) get the bare
  keyword sugar (`test { … }`).
- **Extension tiers** use the **general tagged form** — a *fixed* marker plus the tier *identifier*
  (`dev bench { … }` / `@bench { … }`, spelling TBD), which reserves no new word and is safe to add by
  declaration. The LSP can still render and complete it as nicely as a built-in.
- If an extension tier proves universal, the *language* may promote it to keyword sugar (ship the
  declaration in the prelude) — a language decision, not something an extension does.

So: open-ended, LSP-native tiers via `@dev` library declarations; bare-keyword sugar reserved for the
language's blessed handful.

## Block ergonomics

- **Multiple blocks per file** — drop a `test { }` right under each function it exercises; the runner
  flattens every block into one set (test-fn names unique per file, which the checker already enforces).
  Optional labeled block (`test "arithmetic" { … }`) purely for runner output grouping.
- **Unit vs integration** — in-source `test {}` (private access) vs a separate test-tier file
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

## Open questions (settle before slicing)

1. **Generics** across the kinds (`struct Box<T>`, `class Cell<T>`, `enum Option<T>`) — mostly unchanged,
   but confirm variance/erasure interplay with reference identity for `class<T>`.
2. **Visibility** details — `pub` fields vs private-literal-by-default for classes (encapsulation); does
   a struct field of `class` type force the struct boxed? (Yes — expected.)
3. **`mut` bindings vs reference classes** — `mut x` (rebind the binding) vs `x.f = v` (mutate through
   the reference). For a reference class, `x.f = v` needs no `mut` binding (it mutates the shared
   instance, not the binding); for a value struct it's a reassignment (needs `mut`). Spell this out — it
   diverges from Phase 5.2's struct rule.
4. **Exact spellings** — `@dev` vs `@tier`; the general block form (`dev <tier> { }` vs `@<tier> { }`);
   tuple access (`.0` vs a method).
5. **Enum-body** — land with the redesign, or as an independent follow-on (it's self-contained).

## Suggested slicing

1. **Rename + unify declaration syntax** (`record`→`struct`, retire `type X={}`, one body grammar). Pure
   surface; large but mechanical.
2. **class = reference** (the semantic core): identity, `===`, `!Send`, in-place shared mutation; move
   the value-mutation machinery to `struct`. The load-bearing slice.
3. **Enum bodies** (methods + in-body impls). Self-contained.
4. **Tuples** (value, positional, destructuring, patterns).
5. **Field defaults** (opt-in per field). Independent.
6. **Dev-tier blocks** — the primitive (`@dev` tiers + content-kind + DCE gate + manifest) with **`test`
   first**, then `bench`/`doc`.
