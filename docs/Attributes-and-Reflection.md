# Attributes & Reflection

The language distinguishes **codegen directives** (`@…`) from **data attributes** (`#[…]`), and exposes a small runtime reflection surface. The one-line rule:

> `@` = the compiler generates or registers something; `#[…]` = inert data metadata is attached.

## The decorator directives

Four `@` decorators attach metadata to or drive codegen on a *declaration* — `@derive`, `@attribute`, `@role`, and `@semantic`. (The layout directive `@packed` and the `@test`/`@bench`/`@doc`/`@debug` [dev-tier blocks](Dev-Tiers) also use `@` but do different jobs — see [Other `@` directives](#other--directives) below.)

### `@derive(...)` — synthesize trait impls

Generates trait implementations from a type's shape. Covered in [Generics & Traits](Generics-and-Traits#derive--synthesized-implementations). The **built-in recipe set** is closed — `Equatable`, `Comparable`, `Display`, `Error`, `Clone`, `Serialize<Format>` — but **user traits are derivable too**: a fully-defaulted trait's defaults are adopted wholesale (the `Inspectable` example below), and a required method can be bridged onto a field/method or delegated `via:` a field. What `@derive` never does is run arbitrary code — every synthesized method is a mechanical bridge, forward, or default.

### `@attribute` — mark a struct usable as `#[...]`

A `#[Foo(...)]` attribute *is* a struct constructed in annotation position. The struct opts in by being marked `@attribute`:

```noeta
@attribute
struct Route { path: string  method: string = "GET" }

#[Route("/users")]                 // path: "/users", method defaults to "GET"
struct Users { id: int }

#[Route("/admin", method: "POST")]
fn admin_handler(): void { /* handle the request */ }
```

- Attributes are **structs, not classes** (a struct has one canonical all-fields construction).
- Arguments map to fields — positional in declaration order, or named. A field with a default is optional.
- Using an unmarked struct as an attribute is E0029. Writing `@attribute` itself on a class or
  enum is a misplaced directive, E0054.
- Placement can be constrained by listing target kinds — `@attribute(Method, Function)` — and a misplaced attribute is E0030. The kinds are `Struct`, `Class`, `Enum`, `Function`, `Method`, `Field`, `Variant`.
- Arguments are a **constant literal tree** — scalars, lists, maps, sets, enum values, nested struct literals, and a type reference (which becomes a reflection `Type` value). A non-literal argument (e.g. `1 + 2`) is E0003.
- A type-reference argument keeps its **generic arguments** at full fidelity — `#[Builds(target: List<int>)]` reflects as `Type.List(Type.Int)`, not an erased `Type.List(Type.Dyn)` — and is validated exactly like a type annotation anywhere else: an unknown name inside it is E0013, and a built-in constructor applied at the wrong arity (`List<int, string>`) is E0058.

The `#[Skip]` / `#[Name]` / `#[Group]` / `#[Data]` attributes used by the [test runner](Testing) are exactly such `@attribute` structs. They are **not prelude** — they live in `std.test`, so bring them in with `use std.test.{Skip, Name, Group, Data}` at the top level or qualify one inline (`#[std.test.Skip]`). Without either, `#[Skip]` is the ordinary "`Skip` cannot be used as an attribute" error.

### `@role(Enum.Variant)` — a semantic role tag

Roles exist to make **architecture queryable**: they tag declarations with typed architectural meaning — this is an entry point, that crosses a trust boundary — that tooling can index and answer questions about, from `roles_of()` in-language to the manifest `noeta mcp` serves to AI agents (see [Where this is headed](#where-this-is-headed)).

`@role` rides on an `@attribute` struct and confers a typed architectural role on every declaration the attribute annotates, indexed at build time (zero runtime cost). Only a struct marked `@attribute` may carry `@role`, and the variant must be fieldless.

```noeta
@attribute(Function, Method)
@role(Semantic.EntryPoint)
struct Route { path: string }
```

### `@semantic` — promote an enum to a role vocabulary

Marks an **enum** (only) as a source of role variants. The language ships a built-in `Semantic` enum (`EntryPoint`, `PersistenceBoundary`, `TrustBoundary`, `Sink`, `Layer`); any project enum marked `@semantic` becomes role-eligible:

```noeta
@semantic enum WebRole { Controller; Middleware; ErrorHandler }
```

Applying `@role`/`@semantic` to the wrong declaration kind is E0054 — the one code every
misplaced directive reports, whichever directive it is.

### Other `@` directives

Two more directive families use the `@` sigil but are not decorators in this four-set:

- **`@packed` / `@packed(Layout.Column)`** — a *layout* directive marking a struct as a packed value type (flat or column-major storage). See [Fixed-Width Integers & Packed Types](Fixed-Width-Integers#packed-value-types--packed).
- **`@test` / `@bench` / `@doc` / `@debug`** — *dev-tier* blocks that gate co-located content. See [Dev Tiers](Dev-Tiers).

## The reflection surface

A handful of prelude functions expose type and metadata at runtime — no import needed.

### `fields_of(value)` — value-level field reflection

`fields_of(value)` returns a struct/class instance's fields as `List<FieldEntry>` — each `{ name: string, value: dyn }`, in declaration order (any other value yields the empty list). It is the value-level counterpart of `type_of`, and what lets a fully-defaulted trait implement *structural* behavior over `self` in pure Noeta — no macro system:

```noeta check
trait Inspectable {
    fn inspect(): string {
        mut out = "{"
        for f in fields_of(self) { out = out ~ " " ~ f.name ~ ": " ~ f.value }
        return out ~ " }"
    }
}
@derive(Inspectable)
struct User { name: string; id: int }
```

### `traits_of(value): List<string>` — trait-membership reflection

`traits_of(value)` returns the trait names the value's nominal type has a **registered implementation** for — a standalone `impl Trait for T`, an in-body `impl` block, a `@derive(Trait)`, or a native type's ABI-declared impl — as a sorted, deduped `List<string>`. It reads the same membership table the precise `x is dyn Trait` test consults, so the two can never disagree. Names are **qualified**: a `.noe` trait keeps its linked name (bare for a program-local trait, `App.Models.Trait` for a namespaced module's), and a native trait reports its qualified identity (`std.vec.Kernels`). Built-in traits appear under their bare names (`Comparable`, `Display`) when a type registers an impl or derive of them; built-in *base* types (int, string, `List`, …) carry no declared impls, so `traits_of(42)` is `[]` even though built-in protocol behavior (echo, `<`) works on them. A non-nominal value (scalar, collection, function) yields the empty list — the same "nothing to report" answer `fields_of` gives a non-object.

```noeta
trait Speaks { fn speak(): string }

@derive(Comparable)
struct Dog { age: int }
impl Speaks for Dog { fn speak(): string { return "woof" } }

echo traits_of(Dog { age: 3 })   // ["Comparable", "Speaks"]
echo traits_of(42)               // []
```

### `type_of(value): Type`

Returns the value's runtime head-constructor as the prelude `Type` ADT, which you can `match`:

```noeta
echo match type_of(5) {
    Int    => "int",
    String => "string",
    _      => "other",
}
```

The payload-free cases are spelled **bare** here: the scrutinee is a `Type`, so `Int` and `String` resolve to that enum's own cases rather than binding the whole value (see [pattern matching](Control-Flow-and-Pattern-Matching#a-bare-identifier-is-a-variant-when-the-scrutinees-enum-has-one)). `Type.Int` still works and means the same; reach for it when the short name would read ambiguously. The payload-carrying cases are call-shaped and never needed the qualifier: `List(inner)`, `IntN(bits, signed)`, `Struct(name, args)`.

`Type` variants include the scalars `Type.Int`, `Type.Float`, `Type.F32`, `Type.F64`, `Type.IntN(bits, signed)`, `Type.Bool`, `Type.String`, `Type.Bytes`, `Type.Unit`, `Type.Dyn`; the containers `Type.List(inner)`, `Type.Set(inner)`, `Type.Map(k, v)`, `Type.Option(inner)`, `Type.Result(ok, err)`; `Type.Fn(params, ret)` and `Type.Union(members)`; the trait object `Type.DynTrait(name)`; and the nominals `Type.Struct(name, args)`, `Type.Enum(name, args)`, `Type.Class(name, args)`, `Type.Named(name, args)`. Collection literals carry their resolved element type as a runtime tag that survives a `dyn` launder (a content-changing op like `.set` drops the tag to head-only).

### `type_name::<T>(): string`

A type's **qualified runtime identity**, as a `string` — the same name `type_of` reports inside `Type.Struct(name, args)`, and the same key the name-keyed queries (`field_specs_of(name)`, `variants_of(name)`, `construct(name, …)`, `invoke(name, …)`) are stored under. It is how you *write* that key without hand-writing it:

```noeta
namespace app.storage

pub struct Todo {
    pub id: int
}

echo type_name::<Todo>()                        // app.storage.Todo
echo field_specs_of(type_name::<Todo>()).len()  // 1
```

Turbofish only — a `type_name(s)` taking a runtime string would be the identity function on `s`. The value of the surface is that the **compiler** resolves the type: the name follows a `namespace`, a `use … as` alias, and a rename, none of which a string literal does. A name-keyed repository (`Repository.new(type_name::<Todo>(), "todos", "id")`) is the motivating case — before this it had to be handed `"app.storage.Todo"` spelled out by hand, and nothing checked it.

An unresolvable type is `E0013`, exactly as in any other annotation. A **type parameter** resolves wherever the instantiation actually reaches the site — a top-level generic function's parameter and a generic type's parameter in an instance method both do — and is `E0058` where neither channel does; see below.

### Reflection over a type parameter

`field_specs_of::<T>()`, `variants_of::<T>()` and `construct::<T>(…)` are keyed on a type **name**, and their turbofish arm is a *compile-time* key — resolved like an annotation, folded to a constant. One compiled body serves every instantiation, so inside `fn f<T>()` there is no constant to fold to. That is `E0058`.

```noeta error
fn count_of<T>(): int {
    return field_specs_of::<T>().len()   // E0058 — the turbofish arm is a compile-time key
}

echo count_of::<int>()
```

The fix is to hand the query a **name** instead of a type, through the runtime-string arm each of them already has — and inside a generic function that name is available, because `type_name::<T>()` [forwards](Generics-and-Traits#asking-what-t-is-called):

```noeta check
struct Todo { id: int }

fn count_of<T>(): int {
    return field_specs_of(type_name::<T>()).len()
}

echo count_of::<Todo>()      // 1 — the real schema, per instantiation
```

The `E0058` help says exactly this wherever that route is open. Where it is not — a generic **type**'s parameter inside a method, which reaches the body as a name but not as a recipe — the older advice stands: reflect where the type is concrete and pass the result in, taking a `List<FieldSpec>` (or a `string`) as a parameter and letting the caller supply it. A generic **method's own** `<U>` forwards like a function's, so it needs none of that.

Two reflection turbofishes take a forwarded type parameter directly, both riding the hidden type-argument slot a generic call already carries: `attributes_of::<T>()`, which reads the slot's name to key the manifest, and `type_name::<T>()`, which *is* that name. The rest key on a compile-time constant by design.

### The prelude enums are ordinary enums

`Type` is one of five enums the language declares for you — `Ordering` (what `.compare()` returns), `Type`, `Semantic` (the built-in role vocabulary), `Layout` (the `@packed` storage vocabulary), and `Cancelled` (the `Err` payload of a cancelled `join`). Each is namable like any enum you declare yourself: you can annotate with it, `match` on it exhaustively, **and construct a case by name**. A constructed case is the very same value the runtime hands you, so `==` works and you are not forced into a `match` just to ask one question:

```noeta
echo type_of(5) == Type.Int                   // true
echo type_of([1]) == Type.List(Type.Int)      // true
echo 5.compare(2) == Ordering.Greater         // true
```

The prelude **structs** are ordinary structs in the same way — `Attributed`, `RoleBinding`, `ParamInfo`, `FieldEntry`, `FieldSpec`, `VariantSpec`, `TierRoot`, `TierText` are constructible by literal, and a constructed one equals the materialized one field for field:

```noeta
struct P { a: int; b: string }

echo fields_of(P { a: 1, b: "x" })[0] == FieldEntry { name: "a", value: 1 }   // true
```

(`ParamInfo` and `FieldSpec` are the exception a literal cannot currently spell: their `type` field collides with the `type` keyword in struct-literal position. Reading `p.type` off one works.)

Each shadows like any prelude name: declaring your own `enum Ordering` or `struct FieldEntry` replaces it for that program.

A **native** enum — one an extension registers, like `std.http`'s `Framing` — behaves the same, and does so under either spelling. A leaf import binds the short name; a group import lets you dot into the namespace, which is the spelling you need when two packages export the same short name:

```noeta
use std.http
use std.http.{Framing}

echo http.Framing.Sse == Framing.Sse   // true — one type, two spellings
```

### `params_of(name): List<ParamInfo>`

Reflects a **callable's parameters** by name — one `ParamInfo` per parameter, in declaration order: `{ name: string, type: Type, optional: bool, attrs: List<dyn> }`. The name is a top-level function's bare name or a method's qualified `Type.method` (the same target keying the attribute manifest). `type` is the parameter's *declared* type as the same `Type` ADT `type_of` returns, `optional` reports whether a call may omit the parameter (it declared a default), and `attrs` holds the parameter's own `#[...]` attribute instances:

```noeta
fn scale(factor: f64, xs: List<f32>, ns: List<i32>, label: string = "x"): void { return }

for p in params_of("scale") {
    echo "${p.name}: ${p.type} optional=${p.optional}"
}
// factor: Type.Float optional=false
// xs: Type.List(Type.F32) optional=false
// ns: Type.List(Type.IntN(32, true)) optional=false
// label: Type.String optional=true
```

**Declared types and fixed widths.** `params_of` and `type_of` answer with the *same* `Type` for the same declared type — they share one decoder, so they cannot drift. A runtime scalar carries no width tag, so at **top level** a declared fixed-width scalar erases exactly as its value does: every `iN`/`uN` parameter reflects `Type.Int` and `f64` reflects `Type.Float`, while `f32` is reified and keeps `Type.F32`. In **container-element position** a width is a physically distinct storage slot and is preserved at any depth: `List<i32>` reflects `Type.List(Type.IntN(32, true))`. The practical consequence: matching a signature from `params_of` against runtime values (dependency injection, CLI/router derivation) works for every scalar width — `type_of(5)` is `Type.Int`, and so is an `i32` parameter's `type`. See [Fixed-Width Integers](Fixed-Width-Integers) for the erasure model.

### `returns_of(name): ?Type`

The other half of the same signature index: a callable's **declared return type**, keyed by exactly the string `params_of` takes (a bare fn name, or a qualified `Type.method`). It is what makes a signature reflectable *end to end* — a framework deriving an OpenAPI spec from controller methods reads the request shape out of `params_of` and the response shape out of `returns_of`.

```noeta
struct Repo {
    id: int
    fn find(key: string): ?int { return some(self.id) }
}

class UsersController {
    seen: int
    fn new(): UsersController { return UsersController { seen: 0 } }
    fn create(req: string): List<string> { return [req] }
    fn purge(): void { return }
}

fn describe(target: string): string {
    return match returns_of(target) {
        some(t) => "${t}",
        none    => "no such callable",
    }
}

echo describe("Repo.find")               // Type.Option(Type.Int)
echo describe("UsersController.create")  // Type.List(Type.String)
echo describe("UsersController.purge")   // Type.Unit
echo describe("UsersController.crate")   // no such callable  (a typo, not a void method)
```

The result is a `?Type`, and the option is the point. `params_of` answers an unknown target with the empty list because an empty parameter list is a legitimate answer — folding "unknown" into it loses nothing. A return type has no such spare value: `void` is a real answer (`some(Type.Unit)`), so an empty one would make a mistyped target indistinguishable from a `void` method — precisely the silently-vanishing route a reflection-driven framework has to be able to detect. Hence `none`, which you have to look at.

The `Type` comes out of the same decoder `ParamInfo.type` goes through, so a signature's parameters and its return can never disagree about how a declared type spells — including the kind-agnostic `Type.Named(name, [])` a declared struct/class/enum annotation reflects as. A trait's abstract method signature is indexed too, under `Trait.method`. An `async fn f(): T` reports `T`, the type written in the declaration, not the `Future<T>` a call to it evaluates to — reflection reports declared types throughout.

### `field_specs_of::<T>(): List<FieldSpec>` / `field_specs_of(name): List<FieldSpec>`

The **type-level** field schema of a declared struct or class — one `FieldSpec` per field in declaration order: `{ name: string, type: Type, optional: bool }`. It is the declaration-side twin of `fields_of`: that one reflects an *instance*'s field **values** (and so sees the runtime-erased type), this one reflects the **declaration**, so `type` is precise and `optional` reports whether the field declared a default. An unknown name, or an enum, yields the empty list — an enum's cases are `variants_of`'s answer, and the two are meant to be asked as a pair.

Two surfaces, one node: the turbofish `field_specs_of::<T>()` when you know the type statically, and `field_specs_of(name)` when you hold it only as a runtime string (a `Type.Struct(name, _)` you just reflected). They converge on one name-keyed query, so both behave identically — including under a `namespace`, where the turbofish resolves `T` to the same **qualified** identity `type_of` reports (`field_specs_of::<Todo>()` inside `namespace app.storage` asks for `app.storage.Todo`). The string surface takes the name verbatim, so it wants that qualified name too.

```noeta
struct ServerOpts {
    port: int
    host: string = "localhost"
    verbose: bool = false
}

for spec in field_specs_of::<ServerOpts>() {
    echo "${spec.name}: ${spec.type} optional=${spec.optional}"
}
// port: Type.Int optional=false
// host: Type.String optional=true
// verbose: Type.Bool optional=true
```

### `variants_of::<T>(): List<VariantSpec>` / `variants_of(name): List<VariantSpec>`

The **type-level** variant schema of a declared enum — one `VariantSpec` per variant in declaration order: `{ name: string, payload: List<FieldSpec>, backing: ?dyn }`. It is the enum half of the same declaration-side query `field_specs_of` is the struct half of: two surfaces (turbofish and runtime string), one name-keyed query, the same **qualified**-identity resolution under a `namespace`, and the same lenient answer for a name it does not recognize — a struct, a class, or an unknown name yields the empty list, so a framework can probe any name without a guard.

Ask them **together**, because neither alone can describe an arbitrary type. `field_specs_of` answers an enum with the empty list, and a field-less struct with the empty list too, so through that query alone an enum is indistinguishable from an empty struct: a schema builder that walked a `Type.Named(name, _)` recursed into an enum-typed field, found nothing, and emitted an empty object — silently wrong rather than loudly missing. With the pair, fields present means a struct or class, variants present means an enum, and both empty is the one honest "nothing is known about this name":

```noeta
enum Sentiment { Positive; Negative }

struct Review {
    text: string
    mood: Sentiment
}

for spec in field_specs_of::<Review>() {
    echo "${spec.name}: ${spec.type}"
}
// text: Type.String
// mood: Type.Named(Sentiment, [])   // a name — ask both queries about it

echo field_specs_of("Sentiment").len()   // 0 — not a struct
echo variants_of("Sentiment").len()      // 2 — an enum, and here are its cases
```

A variant's **payload** is reported as ordinary declared-field data, through the very same `FieldSpec` the struct side uses — a payload *is* a field list. A positional payload carries a synthesized `_0`/`_1` name with its real declared type in the type slot, so a positional and a named payload read alike and need no special case at the consumer. `optional` is always `false`: a variant payload field has no syntax for a default, so it can never be omitted.

```noeta
enum Shape {
    Circle(r: float)
    Rect(int, int)
    Many(List<int>)
}

for v in variants_of::<Shape>() {
    echo v.name
    for p in v.payload {
        echo "  ${p.name}: ${p.type}"
    }
}
// Circle
//   r: Type.Float
// Rect
//   _0: Type.Int
//   _1: Type.Int
// Many
//   _0: Type.List(Type.Int)
```

`backing` is the variant's value in a **backed enum** (`enum Status: string`), as `some(value)` — the wire value a derived schema should emit rather than the variant name. A plain enum's variants report `none`, so the `?` is the difference between "backed by this" and "not backed at all":

```noeta
enum Status: string {
    Pending = "pending"
    Done = "done"
}

for v in variants_of::<Status>() {
    echo "${v.name} = ${v.backing}"
}
// Pending = some(pending)
// Done = some(done)
```

A variant's own `#[...]` attributes are deliberately **not** in the report — exactly as a field's are not in `FieldSpec`. A member's attributes are already keyed in the manifest under its qualified `Enum.Variant` target, the same `Type.field` convention the struct side uses, so `attributes_of::<T>()` is the one answer to "what is annotated on this member" for fields, methods, parameters and variants alike. Carrying them here would make the enum half of one surface answer a question the struct half does not, and give a consumer two places to look for one fact:

```noeta
@attribute
struct Doc { text: string }

enum Sentiment {
    #[Doc("a good review")]
    Positive
    Negative
}

for a in attributes_of::<Doc>() {
    echo "${a.target}: ${a.value.text}"   // Sentiment.Positive: a good review
}
```

### `construct::<T>(fields): Result<dyn, string>` / `construct(name, fields): Result<dyn, string>`

Builds a struct, class, or **enum case** value **at runtime** from field values, through the *same* construction path a literal takes — so field defaults and full-initialization are honored identically, and a type that appears in no literal anywhere in the program still constructs. Like `field_specs_of` it has a turbofish and a runtime-string surface (with the same qualified-identity resolution under a `namespace`); like `invoke` it is fallible by construction, returning a `Result` rather than aborting. Both surfaces are typed `Result<dyn, string>` — the turbofish only spells the type *name*, so narrow the `Ok` payload back with `.as<T>()` when you need the static type.

`fields` accepts either shape:

- a **`List<dyn>`** — positional, in declaration order. A list shorter than the field count fills the remaining fields from their defaults, so trailing optional fields may simply be left off. It cannot express a *gap* (an omitted middle field), by design.
- a **`Map<string, dyn>`** — named, sparse and in any order. This is the form a CLI expanding a struct into `--field` flags produces: supply `port` and `verbose` and let the middle `host` fall back to its default.

```noeta
struct ServerOpts {
    port: int
    host: string = "localhost"
    verbose: bool = false
}

mut named: Map<string, dyn> = {}
named["port"] = 3000
named["verbose"] = true

match construct::<ServerOpts>(named) {
    Ok(v) => {
        o = v.as<ServerOpts>() ?? ServerOpts { port: 0 }
        echo "${o.port}/${o.host}/${o.verbose}"    // 3000/localhost/true
    },
    Err(e) => {
        echo e
    },
}
```

Every rejection is an `Err(string)` carrying a ready-to-surface message, never an abort:

| Situation | Message |
|---|---|
| the name is not a string | ``construct type name must be a string, found <kind>`` |
| the name is a bare enum | ``` `Sentiment` is an enum: name the variant to construct, as in `construct("Sentiment.Positive", […])` ``` |
| the name is `Enum.Variant` but the enum has no such case | ``` `Sentiment` has no variant `Sideways` ``` |
| the name is nothing the program declares | ``` `Foo` is not a constructible type ``` |
| `fields` is neither a list nor a map | ``construct fields must be a list or a map, found <kind>`` |
| more positional values than fields | ``` `Foo` has 2 field(s), but 3 value(s) were given ``` |
| a named field the type does not have | ``` `Foo` has no field `nope` ``` |
| a value whose scalar kind disagrees with the declared field type | ``` field `port` of `Foo` expects int, got string ``` |
| a field that is neither supplied nor defaulted | ``` missing required field `port` of `Foo` ``` |

Validation runs through one shared planner, so both backends accept, reject, and *word* every case identically.

#### Constructing an enum case

An enum case is spelled `construct("Enum.Variant", payload)` — the case goes where the type name goes, exactly as it is written in source, and `fields` is that variant's **payload**.

```noeta
enum Shape { Circle(r: int); Rect(w: int, h: int); Dot }

echo match construct("Shape.Rect", [2, 5]) {
    Ok(v) => "${v}",                    // Shape.Rect(2, 5)
    Err(e) => e,
}
```

The payload takes the same two shapes a struct's fields do, and means the same things: a positional `List<dyn>` in declaration order, or a `Map<string, dyn>` keyed by the payload's field names. Those names are the ones [`variants_of`](#variants_oft-listvariantspec--variants_ofname-listvariantspec) reports — a named payload's declared names, or the synthesized `_0`/`_1` of a positional one — so the query that tells you what a case needs and the call that builds it speak one vocabulary:

```noeta
enum Shape { Circle(r: int); Rect(w: int, h: int); Dot }

for v in variants_of("Shape") {
    mut names: List<string> = []
    for f in v.payload { names = names ~ [f.name] }
    echo "${v.name}(${names.join(", ")})"     // Circle(r) / Rect(w, h) / Dot()
}
echo match construct("Shape.Rect", {"w": 2, "h": 5}) {
    Ok(v) => "${v}",                    // Shape.Rect(2, 5)
    Err(e) => e,
}
```

A payload is validated against that declared schema by the very planner a struct's fields go through, so a wrong scalar kind and a missing payload field are worded exactly like their struct counterparts (`field `r` of `Shape.Circle` expects int, got string`).

A **bare enum name** is a rejection rather than a second spelling. The alternative — `construct("Shape", ["Circle", 3])`, with the case name smuggled in as the first value — would make the field list mean two different things depending on position, and leave no spelling at all for a fieldless case that reads like a payload-carrying one. The same goes for the turbofish: `construct::<Shape>(…)` spells only a type *name*, so it reports the same teaching error.

To go the other way — a single **wire value** to a case, rather than a payload to a variant — use [`Enum.try_from`](Structs-Classes-and-Enums#enums), which matches a backed enum's backing.

### `attributes_of::<T>(): List<Attributed<T>>`

Materializes every `#[T(...)]` attribute in the program — each entry's `.value` is a real `T`, and `.target` is the annotated declaration's name:

**"In the program" means every file the program is built from**, not only the declarations something imported. A data attribute is a **link root**: an annotated declaration in a sibling module, or in a dependency package, is part of the program whether or not any `use` names it — which is the whole point of tagging a function for discovery. So a `#[Tool]`-scanning framework finds the tools nothing statically references, and finds them by their **qualified** target name (`app.tools.run`, matching `type_of`'s naming under a `namespace`). Visibility does not gate discovery either: a module-private `#[Tool] fn` is a registration, and `invoke(a.target, args)` really calls it — reflection and dispatch see the same set by construction. What the rule does *not* do is drag in unannotated code: a sibling's unannotated function that nothing imports stays out of the program, exactly as before.

```noeta check
@attribute
struct Route { path: string }

#[Route("/users")]
fn list_users(): string { return "…" }

routes = attributes_of::<Route>()
for r in routes {
    echo "${r.target} -> ${r.value.path}"
}
```

A type-reference argument arrives as a full reflection `Type` value, generic arguments intact — what a codegen or DI consumer needs to reconstruct the declared type, not just its head:

```noeta
@attribute
struct Builds { target: Type }

#[Builds(target: List<int>)]
fn make_list(): List<int> { return [] }

for b in attributes_of::<Builds>() {
    echo "${b.target}: ${b.value.target}"    // make_list: Type.List(Type.Int)
}
```

### `roles_of(): List<RoleBinding>` / `roles_of::<RoleEnum>(): List<RoleBinding>`

The compile-time `(declaration, role)` index built from `@role(...)` tags — each binding has a `.target` and a `.role`. The optional turbofish scopes the query to a single `@semantic` enum (the mirror of `attributes_of::<T>()`): `roles_of::<Semantic>()` returns only the bindings whose role is a `Semantic` variant, while bare `roles_of()` returns the whole index. The enum is resolved at compile time (closed-world); naming a non-`@semantic` type is an error (E0031).

It reads the same manifest `attributes_of` does, so it has the same reach: every annotated declaration in the program, including ones no `use` names, and including a role conferred by a *dependency package's* `@role`-bearing attribute. `.role` is a real enum value, so it compares directly — `if b.role == Semantic.TrustBoundary { … }`.

### `invoke(recv, name, args): Result<dyn, dyn>` / `invoke(name, args): Result<dyn, dyn>`

Fallible dispatch by name. With three operands, `recv` is a value (→ an instance method) or a bare type name (→ an associated function):

```noeta
struct Rect {
    w: int
    h: int
    fn new(w: int, h: int): Rect { return Rect { w: w, h: h } }
    fn area(): int { return self.w * self.h }
}

echo match invoke(Rect.new(2, 3), "area", []) {
    Ok(v)  => "area = ${v}",             // area = 6
    Err(e) => "no such method",
}
```

With two, `name` is a **top-level function** — the same string `params_of` takes for a free fn, so reflecting a signature and then calling it round-trips on one name:

```noeta
fn greet(who: string = "stranger"): string { return "hi ${who}" }

echo match invoke("greet", ["ada"]) {
    Ok(v)  => v,                         // hi ada
    Err(e) => "no such function",
}
```

The two-operand form searches the top-level function namespace and nothing else. A type name, a qualified `Type.method`, and a local variable holding a function are each simply not found — reaching a type's methods is what the three-operand form is for, and a function value you already hold you can just call.

`args` accepts either shape, exactly as `construct`'s `fields` does, and in both the two- and three-operand forms:

- a **`List<dyn>`** — positional, in declaration order. A list shorter than the parameter count leaves the remaining parameters to their defaults, so trailing optional parameters may simply be left off. It cannot express a *gap*, by design.
- a **`Map<string, dyn>`** — named, sparse and in any order. This is the form a caller filling a signature from `params_of` produces: supply `a` and `c` and let the middle `b` run its default.

```noeta
fn place(item: string, qty: int = 1, note: string = "-"): string {
    return "${item} x${qty} (${note})"
}

mut call: Map<string, dyn> = {}
call["item"] = "widget"
call["note"] = "rush"

echo match invoke("place", call) {
    Ok(v)  => v,                         // widget x1 (rush)
    Err(e) => e,
}
```

The omitted `qty` runs its compiled default expression, exactly as at a direct `place(item: "widget", note: "rush")` call site — the named form is the same calling convention reached by name, not a second one. Parameter names come from the same signature index `params_of` reads, so reflecting a signature and then calling it by name round-trips on one target string.

Every rejection is an `Err`, never an abort — an unknown name, a non-string name, args that are neither a list nor a map, an arity mismatch, and, in the named form:

| Situation | Message |
|---|---|
| `args` is neither a list nor a map | ``invoke args must be a list or a map, found <kind>`` |
| a named argument the callable has no parameter for | ``` `place` has no parameter `nope` ``` |
| a parameter that is neither supplied nor defaulted | ``` missing required parameter `item` of `place` ``` |
| a callable the signature index does not describe (a global holding a closure that was never declared as a `fn`) | ``` `f` does not take named arguments ``` |

Unlike `construct`, the named form does **not** type-check an argument against its declared parameter type. `invoke`'s positional form never has — the callee's own typing is the backstop — and checking one form but not the other would make the very same call succeed positionally and fail by name.

A parameter with a default may be omitted from either shape, exactly as at a direct call site — the pair to `ParamInfo.optional`. A panic *inside* the invoked body is a normal abort; only the by-name resolution is caught.

## Where this is headed

The reflection manifest — declarations, their `#[…]` attributes, and their `@role`/`@semantic` tags — backs an agentic tooling surface: `noeta mcp` serves this manifest over stdio (roles, attributes, and the architectural graph) to MCP clients. See [Editor & AI Tooling](Editor-and-AI-Tooling) for the tool inventory.
