# Structured attribute arguments: the literal value tree (+ type references)

Status: **DONE** (two commits: `079956b` literal tree, `00b1257` type-ref + invoke; conformance 213 /
differential 206 matched / 0-skipped / backends agree / clippy + fmt + miri clean). Branch
`types-inferred-static`. Generalized attribute arguments from scalars to a full **constant literal
tree**, building the A2-era "nested record-valued attribute args" deferral. Independent of the
roles/abstract-types plans.

**As built:** `AttrValue` is now recursive (List/Set/Map/Enum/Record/TypeRef; `Ident` replaced). The
parser **reuses the full expression grammar** then folds the result into an `AttrValue` via
`expr_to_attr_value` (one literal grammar, no parallel parser; a non-literal node → E0003 at parse
time; set sugar `#{..}`→`[..].to_set()` recovered structurally; bare name → `TypeRef`, `none`/`Ok`/
`Err`/`some` → enum constructors, qualified `Enum.Variant` → enum). The checker `check_attr_value`
type-checks the tree **recursively** against each field type (list/set elements, map values, record
fields, enum/type-ref by assignability; E0007; `dyn` stays lenient); a `TypeRef` must name a real
type (E0013). Both backends' `attr_value_to_eval`/`attr_value_to_vm` recurse to build composite
values (sets canonicalized like `to_set`), differential-clean by construction. A `TypeRef`
materializes as the reflection `Type` ADT (`Type.Named(name,[])`), and **`invoke` accepts a `Type`
receiver** (resolves the named type → associated-fn dispatch; both backends extract the name from the
`Type` value), so a stored type-ref is constructible. **Deferred (noted, not silently):** retiring
P2.6's `Payload::Type` in favor of the ADT (its degenerate name-only case); classifying a type-ref's
*kind* (it materializes as `Type.Named`, not `Type.Record`/etc.) — a possible later refinement.

## What and why

Today `#[Foo(...)]` arguments are scalars + a bare identifier (`AttrValue = Str | Int | Float | Bool
| Ident`). To let attributes carry real structure, generalize the argument value to a **recursive
literal tree**: scalars plus the collection and nominal literals, composed arbitrarily. This is
exactly Java/C# annotation arguments (constants + arrays + nested annotations + enum + `Class`) —
"config data with nominal types."

```
#[Endpoint(
    path: "/users",
    methods: [Method.Get, Method.Post],                 // List of enum values
    limits: { "rps": 100, "burst": 200 },               // Map
    roles: #{Role.Admin, Role.User},                    // Set
    schema: Schema { strict: true, version: 2 },        // nested Record
    codec: JsonConverter,                               // type reference
)]
type Users = { id: int };
```

## The boundary

The one hard rule is **no comptime**: an argument is materialized at manifest-build time without
running user code. So the value is **literals and compositions of literals only** — never an
expression (`1 + 2`), a call, a closure, a range, or anything reading runtime/`self` state.

## The value kinds (the closed grammar)

`AttrValue` becomes a recursive enum:

| Kind | Surface | Notes |
|---|---|---|
| `Str` / `Int` / `Float` / `Bool` | `"x"` `42` `1.5` `true` | unchanged |
| **`List(Vec<AttrValue>)`** | `[a, b, c]` | the most common structured arg (C#/Java center on it) |
| `Map(Vec<(AttrValue, AttrValue)>)` | `{ "k": v }` | |
| `Set(Vec<AttrValue>)` | `#{a, b, c}` | the `#`-prefix disambiguates it from map/record |
| `Enum { enum_name, variant, args: Vec<AttrValue> }` | `Color.Red`, `Ok(5)` | fieldless or literal-payload; **Option/Result come free** (they are enums) |
| `Record { type_name, fields: Vec<(String, AttrValue)> }` | `Point { x: 1 }` | the named type prefix disambiguates it from map |
| `TypeRef(String)` | `JsonConverter` | a bare name → a type reference (see below) |

Everything composes recursively, so a `List` of `Record`s of `Enum`s is a single literal tree.
`Ident` is **replaced**: a qualified `Enum.Variant` is an enum value; a bare name is a `TypeRef`
(resolves the old "enum-like constant by name" ambiguity cleanly).

## Type references as values

`TypeRef("JsonConverter")` lets an attribute field hold *a type* (C# `typeof(Foo)`, Java `Class<?>`).

**Decision (architectural, made — DRY by unification):** a type-reference field is statically typed
as the reflection **`Type`** ADT (P2.2) and materializes to `Type.Named("JsonConverter", [])`. The
reasoning: "a type, as a value" is **one** concept, not two — a user reaching for `type_of(x)`, a
bare type name, or a stored type-ref wants the same first-class type handle. C#'s `System.Type`
unifies descriptive + operational exactly this way and is the right precedent at this scale; Java's
`Type`-vs-`Class<?>` split buys bounded-context purity we don't need and forces the user to learn two
near-identical things. P2.6's `Payload::Type(String)` handle is *not* a separate domain — it was a
name-only shortcut so `invoke` had a receiver; it's the degenerate `Type.Named(name, [])` case.

So there is **one** representation with three producers (`type_of`, a bare type name, a stored
type-ref). The elegant follow-through, folded into this work: let **`invoke` accept a `Type` value as
its receiver** (matching `Type.Named(name, …)` to dispatch), so a stored type-ref can actually be
constructed/dispatched — which is the whole point of storing one. (Optionally retire `Payload::Type`
in favor of the ADT later; not required for this slice.) An unknown type name → `E0013`.

## Mechanism

- **Parser** (`lang-parser`): the attribute-arg value parser becomes a **recursive literal-value
  parser** reusing the existing literal grammars — list `[...]`, map `{k: v}`, set `#{...}`, record
  `Name {...}`, enum `Name.Variant(...)`, scalars — but **restricted to literal sub-values** (no
  arbitrary expressions). Because set is `#{…}` and record carries a named type, the `{…}`
  disambiguation is the same fork expression position already resolves, so this largely reuses
  existing parsers with a "literal-only" mode.
- **AST / manifest** (`lang-ast`, `lang-bytecode` mirror): the recursive `AttrValue` above; the
  `lang-bytecode` `AttributeValue` mirror grows the same arms.
- **Checker** (`lang-check`): `check_attribute_construction` must type each composite literal against
  its field type **recursively** — a list against `List<T>`, a record literal against the field's
  record type, an enum value against the field's enum type, a type-ref against `Type`. This is
  "type-check a literal value tree against a type," largely shared with object-literal checking.
  Reuses `E0007`/`E0009`/`E0005`.
- **Materialization** (`lang-ast::reflect::materialize_args` + `attr_value_to_eval` /
  `attr_value_to_vm`): recurse to build composite values — objects, enums, lists, maps, sets, type
  values — in both backends. Differential-clean by construction (one shared `AttrValue` tree → both
  backends build identically, the `attributes_of` precedent).

## Touch list

`lang-ast` (`AttrValue` recursive + `reflect::materialize_args`), `lang-bytecode` (`AttributeValue`
mirror), `lang-parser` (literal-value parser), `lang-check` (recursive construction check),
`lang-eval` + `lang-vm` (recursive materializers), conformance
`tests/conformance/reflection/attributes_of_*` (list/map/set/record/enum/type-ref args, nested,
read back via `attributes_of`, differential), checker units, `docs/resources/02-syntax.md` §9.7.

## Verification & sequencing

Conformance + differential (0-skipped, agree), workspace/clippy/fmt/miri. Independent of the roles
and abstract-types plans; can land in any order relative to them. Standard trailers.
