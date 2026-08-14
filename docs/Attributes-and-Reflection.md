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
- Using an unmarked struct as an attribute is E0029. Writing `@attribute` itself on a class or enum is a misplaced directive, E0054.
- Placement can be constrained by listing target kinds — `@attribute(Method, Function)` — and a misplaced attribute is E0030. The kinds are `Struct`, `Class`, `Enum`, `Function`, `Method`, `Field`, `Variant`.
- Arguments are a **constant literal tree** — scalars, lists, maps, sets, enum values, nested struct literals, and a type reference (which becomes a reflection `Type` value). A non-literal argument (e.g. `1 + 2`) is E0003.
- A type-reference argument keeps its **generic arguments** at full fidelity — `#[Builds(target: List<int>)]` reflects as `Type.List(Type.Int)`, not an erased `Type.List(Type.Dyn)` — and is validated exactly like a type annotation anywhere else: an unknown name inside it is E0013, and a built-in constructor applied at the wrong arity (`List<int, string>`) is E0058.

The `#[Skip]` / `#[Name]` / `#[Group]` / `#[Data]` / `#[Timeout]` attributes used by the [test runner](Testing) are exactly such `@attribute` structs, as is [`#[Transient]`](Derives#keeping-a-field-out-of-the-wire), which takes a field out of its type's serialized shape. They are **not prelude** — each lives in the module it belongs to, so bring it in with `use std.test.{Skip, Name, Group, Data, Timeout}` / `use std.json.Transient` at the top level, or qualify one inline (`#[std.test.Skip]`). Without either, `#[Skip]` is the ordinary "`Skip` cannot be used as an attribute" error. `#[Transient]` also declares its placement, so writing it anywhere but a field is E0030.

### `@role(Enum.Variant)` — a semantic role tag

Roles exist to make **architecture queryable**: they tag declarations with typed architectural meaning — this is an entry point, that crosses a trust boundary — that tooling can index and answer questions about, from `roles_of()` in-language to the manifest `noeta mcp` serves to AI agents (see [The manifest outside the language](#the-manifest-outside-the-language)).

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

Applying `@role`/`@semantic` to the wrong declaration kind is E0054 — the one code every misplaced directive reports, whichever directive it is.

### Other `@` directives

Two more directive families use the `@` sigil but are not decorators in this four-set:

- **`@packed` / `@packed(Layout.Column)`** — a *layout* directive marking a struct as a packed value type (flat or column-major storage). See [Fixed-Width Integers & Packed Types](Fixed-Width-Integers#packed-value-types--packed).
- **`@test` / `@bench` / `@doc` / `@debug`** — *dev-tier* blocks that gate co-located content. See [Dev Tiers](Dev-Tiers).

## The reflection surface

A handful of prelude keywords expose type and metadata at runtime — no import needed. The rest of this page documents each one; read this section first, because most of them answer a question you may not actually have.

### Choosing a surface

**Reach for a type test before you reach for reflection.** If you hold a *value* and you know the candidate types, `is` is the whole answer: it is checked at compile time, it **narrows** the scrutinee inside the arm, and a `match` over a union is exhaustive without a `_`. Reflection buys you nothing here and costs you the checker.

```noeta
struct Todo { id: int }

fn label(x: dyn): string {
    return match x {
        is Todo      => "todo #${x.id}",     // narrowed — `.id` is legal in this arm
        is List<int> => "${x.len()} ints",    // element-precise, not just "a list"
        _            => "${type_of(x)}",      // reflection is the fallback, not the default
    }
}

echo label(Todo { id: 7 })     // todo #7
echo label([1, 2, 3])          // 3 ints
echo label(4.5)                // Type.Float
```

`type_of` earns its place in that last arm and nowhere else in that function: it is for the case where there **is no candidate set** — you must describe whatever arrives, and you cannot enumerate it in source. See [Type tests and narrowing](Type-System#type-tests-and-narrowing) for `is` and `.as<T>()` in full, and [Open vs. closed matching](Control-Flow-and-Pattern-Matching#open-vs-closed-matching) for why a union needs no `_` and `dyn` does.

With that settled, the question you have picks the surface:

| The question | The surface | Notes |
|---|---|---|
| Is this value a `Todo`? (you can name the candidates) | `x is Todo` / `x.as<Todo>()` | Not reflection. Checked, and it narrows. |
| Does this value's type implement `Store`? | `x is dyn Store` | Same — one known trait, checked. |
| What type is this value, whatever it turns out to be? | [`type_of(x)`](#type_ofvalue-type) | The `Type` ADT, for a walk with no candidate set. |
| What is this type *called*, as a `string`? | [`type_name::<T>()`](#type_namet-string) when you can spell the type; [`t.name()`](#typename-string) on a `Type` you are already holding | Either way the qualified identity — the key every name-keyed query below is stored under. Never hand-write the string. |
| Which traits does this value's type implement — all of them? | [`traits_of(x)`](#traits_ofvalue-liststring--trait-membership-reflection) | A list of names, for a report. `is dyn Trait` for a decision. |
| What fields does this *value* carry, with their values? | [`fields_of(x)`](#fields_ofvalue--value-level-field-reflection) | Runtime-erased types — it sees values. |
| What fields does this *type* declare, with their types and defaults? | [`field_specs_of`](#field_specs_oft-listfieldspec--field_specs_ofname-listfieldspec) | Declared types, precise. Ask with `variants_of`. |
| What cases does this enum declare? | [`variants_of`](#variants_oft-listvariantspec--variants_ofname-listvariantspec) | The enum half of the same query. |
| What does this callable take, and return? | [`params_of`](#params_ofname-listparaminfo) / [`returns_of`](#returns_ofname-type) | Declared types — there is no value to test. |
| Which declarations carry `#[Route]`? | [`attributes_of::<Route>()`](#attributes_oft-listattributedt--attributes_ofname-listattributeddyn) | Whole-program, `use`-independent. |
| Which declarations carry an architectural role? | [`roles_of()`](#roles_of-listrolebinding--roles_ofroleenum-listrolebinding--roles_ofname-listrolebinding) | The compile-time `(declaration, role)` index. |
| Build a value of a type I hold only as a name | [`construct`](#constructtfields-resultdyn-string--constructname-fields-resultdyn-string) | The reflective **struct literal** — [it is not `new`](#construct-is-the-reflective-literal-not-your-constructor). |
| Call something whose name arrived as *data* | [`invoke`](#invokerecv-name-args-resultdyn-dyn--invokename-args-resultdyn-dyn) | The one consumer of the names the rest produce. |
| Rebuild a `List<T>` of a `@packed` type from a `bytes` blob | [`from_bytes::<T>(blob)`](Fixed-Width-Integers#bytes--serialize-a-packed-list) | A typed decode, not a query. Documented with packed types. |

#### The operand tells you which axis you are on

Every surface takes a **value**, a **type** (turbofish), or a runtime **string** — and which ones it accepts is a design statement, not an accident:

| Operand | Surfaces | Why that shape |
|---|---|---|
| a **value** | `type_of`, `fields_of`, `traits_of` | You have the thing; the answer is about what it *is*. |
| a **type**, turbofish only | `type_name::<T>()`, `from_bytes::<T>(b)` | `type_name` has no string arm because it would be the identity function on its argument. `from_bytes` has none because it needs `T`'s packed *layout*, and a name does not carry one. |
| **either** | `field_specs_of`, `variants_of`, `construct`, `attributes_of`, `roles_of` | One name-keyed query, two ways to spell the key: a compile-time constant, or a name you are holding. |
| a runtime **string** only | `params_of`, `returns_of`, `invoke` | No turbofish arm exists, deliberately — the target's name *arrives as data*. |

That last row is the axis worth internalizing. `params_of`, `returns_of` and `invoke` have no static arm at all, because nothing about them is static: the target is a `#[Tool]` manifest entry's `.target`, a router action, an argv subcommand. **The other surfaces produce names; these three consume them.** Adding a turbofish to `invoke` would be adding a slower way to write a call you could already write directly.

The middle row is the whole name-keyed surface, and it is one contract rather than five agreements. Every query in it keys on a type *name*, so both arms end at the same runtime node — which is also what lets the turbofish arm answer for a **type parameter**: generics are erased, but the instantiation's name reaches the body on a per-call channel, and the compiler routes the surface through its own string arm there. See [Reflection over a type parameter](#reflection-over-a-type-parameter).

Reaching for the wrong axis is a **parse** error, `E0003` — `params_of::<Foo>()` fails at the operand, before typing ever runs. That is deliberate: a surface's operand shape is part of what it *is*, so getting it wrong is not a type mismatch to be coerced away.

### When the `Type` ADT earns its keep

Its dominant channel is **not** `type_of`. Across the `para/*` packages that consume reflection for real, `params_of`/`returns_of` outnumber `type_of` by better than five to one — and that is the shape of the work: they reflect *declared* types, where there is no value to `is`-test because the value has not been built yet. A schema generator reads a signature and emits the JSON a caller must send; a DI container reads a signature and decides what to inject. Both are asking about a declaration.

```noeta
fn render(t: Type): string {
    return match t {
        Type.Option(inner) => "${render(inner)} (optional)",
        Type.List(inner)   => "array of ${render(inner)}",
        Type.Int           => "integer",
        Type.String        => "string",
        _                  => "unsupported: ${t}",
    }
}

fn create(id: int, tags: ?List<string>): void { return }

for p in params_of("create") {
    echo "${p.name}: ${render(p.type)}"
}
// id: integer
// tags: array of string (optional)
```

Three properties make the ADT irreplaceable by a name string, and the walk above uses all three:

1. **Most types have no *key*.** Only a nominal — a declared struct, class, or enum — has a name the name-keyed queries can answer for. `Type.Union(members)`, `Type.Fn(params, ret)`, `Type.Option(inner)`, `Type.IntN(bits, signed)` have a head to print but nothing to look up, and their arguments are not in the head at all. A names-as-strings design has to invent a spelling and then re-parse it.
2. **The walk recurses, so `inner` must be a *value*.** `?List<string>` is two constructors deep before it reaches something a schema can name; `render` gets `inner` handed to it and calls itself. A head name would have to be re-resolved at every level.
3. **Exhaustiveness is the totality proof.** `Type` is an ordinary enum, so a `match` over it with no `_` is checked (E0011). That is how a schema-deriving walk *knows* it is total: every shape it cannot represent gets a deliberate error message rather than falling into a catch-all and emitting a silently-wrong empty schema. `para/ai`'s tool-schema walk is written exactly this way — all 23 arms, no `_`, and the unrepresentable ones return `Err` carrying a sentence about what to declare instead ("`bytes` has no tool-argument encoding — take a `string` and decode it in the tool").

So if you write such a walk, write it without a `_` and let the checker list what you missed. The list is longer than it looks: 23 cases, and `Type.Never` is one of them.

### `fields_of(value)` — value-level field reflection

`fields_of(value)` returns a struct/class instance's fields as `List<FieldEntry>` — each `{ name: string, value: dyn }`, in declaration order (any other value yields the empty list). It is the value-level counterpart of `type_of`, and what lets a fully-defaulted trait implement *structural* behavior over `self` in pure Noeta — no macro system:

```noeta
trait Inspectable {
    fn inspect(): string {
        mut out = "{"
        for f in fields_of(self) { out = out ~ " " ~ f.name ~ ": " ~ f.value }
        return out ~ " }"
    }
}
@derive(Inspectable)
struct User { name: string; id: int }

echo User { name: "ada", id: 1 }.inspect()   // { name: ada id: 1 }
```

### `traits_of(value): List<string>` — trait-membership reflection

`traits_of(value)` returns the trait names the value's nominal type has a **registered implementation** for — a standalone `impl Trait for T`, an in-body `impl` block, a `@derive(Trait)`, or a native type's ABI-declared impl — as a sorted, deduped `List<string>`. It reads the same membership table the precise `x is dyn Trait` test consults, so the two can never disagree. Names are **qualified**: a `.noe` trait keeps its linked name (bare for a trait in a package-less script, `shop.models.Trait` for one in a module), and a native trait reports its qualified identity (`std.vec.Kernels`). Built-in traits appear under their bare names (`Comparable`, `Display`) when a type registers an impl or derive of them; built-in *base* types (int, string, `List`, …) carry no declared impls, so `traits_of(42)` is `[]` even though built-in protocol behavior (echo, `<`) works on them. A non-nominal value (scalar, collection, function) yields the empty list — the same "nothing to report" answer `fields_of` gives a non-object.

```noeta
trait Speaks { fn speak(): string }

@derive(Comparable)
struct Dog { age: int }
impl Speaks for Dog { pub fn speak(): string { return "woof" } }

echo traits_of(Dog { age: 3 })   // ["Comparable", "Speaks"]
echo traits_of(42)               // []
```

### `type_of(value): Type`

Returns the value's runtime head-constructor as the prelude `Type` ADT, which you can `match`. Reach for it when you cannot name the candidates — when you *can*, [`x is T`](#choosing-a-surface) is shorter, checked, and narrows:

```noeta
echo match type_of(5) {
    Int    => "int",
    String => "string",
    _      => "other",
}
```

The payload-free cases are spelled **bare** here: the scrutinee is a `Type`, so `Int` and `String` resolve to that enum's own cases rather than binding the whole value (see [pattern matching](Control-Flow-and-Pattern-Matching#a-bare-identifier-is-a-variant-when-the-scrutinees-enum-has-one)). `Type.Int` still works and means the same; reach for it when the short name would read ambiguously. The payload-carrying cases are call-shaped and never needed the qualifier: `List(inner)`, `IntN(bits, signed)`, `Struct(name, args)`.

`Type` has exactly **23** variants, and the whole list matters if you write a `_`-free walk over it: the scalars `Type.Int`, `Type.Float`, `Type.F32`, `Type.F64`, `Type.IntN(bits, signed)`, `Type.Bool`, `Type.String`, `Type.Bytes`, `Type.Unit`, `Type.Dyn`, `Type.Never`; the containers `Type.List(inner)`, `Type.Set(inner)`, `Type.Map(k, v)`, `Type.Option(inner)`, `Type.Result(ok, err)`; `Type.Fn(params, ret)` and `Type.Union(members)`; the trait object `Type.DynTrait(name)`; and the nominals `Type.Struct(name, args)`, `Type.Enum(name, args)`, `Type.Class(name, args)`, `Type.Named(name, args)`. Collection literals carry their resolved element type as a runtime tag that survives a `dyn` launder (a content-changing op like `.set` drops the tag to head-only).

`type_of` reports what a value **is**, which means three variants only ever reach you through the *declaration* channel. `Type.Never` is uninhabited, so no value has it; a value behind a `dyn Trait` binding reports its own concrete type, not `Type.DynTrait`; and a value of a union type reports the member it actually holds, not `Type.Union`. All three are ordinary answers from `params_of`/`returns_of` — the clearest illustration of why that is the ADT's main channel:

```noeta
trait Speaks { fn speak(): string }
struct Dog { name: string }
impl Speaks for Dog { pub fn speak(): string { return "woof" } }

fn boom(): never { panic("unreachable") }
fn handle(pet: dyn Speaks, n: int | string): void { return }

echo returns_of("boom")                                  // some(Type.Never)
for p in params_of("handle") { echo "${p.name}: ${p.type}" }
// pet: Type.DynTrait(Speaks)
// n: Type.Union([Type.Int, Type.String])

// The same two facts from the value side, which reports what the values are:
echo type_of(Dog { name: "Rex" })                        // Type.Struct(Dog, [])
```

### `Type.name(): string`

A reflected type answers **its own head name** — the qualified, argument-free name of the type's head:

```noeta
struct Todo { id: int }

echo type_of(Todo { id: 1 }).name()       // Todo
echo type_of([1]).name()                  // List
echo type_of(5).name()                    // int
echo Type.DynTrait("Greet").name()        // Greet
```

It is the value-side counterpart of `type_name::<T>()` below: the **same qualified identity**, read off a `Type` you are *holding* rather than off a type you can spell. Inside a module `app.storage` that `Todo` reads `app.storage.Todo` — the name `type_of` shows inside `Type.Struct(name, args)`, verbatim and never shortened.

For a **nominal** that name is a *key*: it is what the name-keyed queries (`field_specs_of(name)`, `variants_of(name)`, `construct(name, …)`) are stored under, so a walk can look up a type it only encountered. For every other case it is a name to print, not to look up — a `List` has no schema to fetch, and its element type is not in the head at all (see [When the `Type` ADT earns its keep](#when-the-type-adt-earns-its-keep)).

**Every case answers a non-empty name** — all 23 of them: a nominal its declared name, a container its constructor (`List`, `Map`, `Result`), a scalar its surface spelling (`int`, `f32`, `u8`), and the forms no bare name spells their constructor (`Fn`, `Union`, `unit`, `never`, `dyn`). That totality is the point — the `match type_of(v) { Struct(n, _) => n, _ => "" }` it replaces answers the **empty string** for every case its match forgot, and that empty name then travels on as a table name, a route, or a schema key.

It is a zero-argument method rather than a field because `Type` is an enum, and an enum's accessor surface is a method (`.value()` on a backed enum); it is spelled `name` to match the `name` field the other reflected descriptors carry (`FieldSpec.name`, `ParamInfo.name`, `VariantSpec.name`).

### `type_name::<T>(): string`

A type's **qualified runtime identity**, as a `string` — the same name `type_of` reports inside `Type.Struct(name, args)`, and the same key the name-keyed queries (`field_specs_of(name)`, `variants_of(name)`, `construct(name, …)`, `invoke(name, …)`) are stored under. It is how you *write* that key without hand-writing it:

```noeta
// A single file with no package: nothing derives, so the identity is the bare name. The same
// declaration inside a package `local/app` as `src/storage.noe` derives `app.storage`, and
// `type_name` then reports `app.storage.Todo` — see
// [Modules](Modules#where-a-modules-path-comes-from).
pub struct Todo {
    id: int
}

echo type_name::<Todo>()                        // Todo
echo field_specs_of(type_name::<Todo>()).len()  // 1
```

Turbofish only — a `type_name(s)` taking a runtime string would be the identity function on `s`. The value of the surface is that the **compiler** resolves the type: the name follows the module's path, a `use … as` alias, and a rename, none of which a string literal does. A name-keyed repository (`Repository.new(type_name::<Todo>(), "todos", "id")`) is the motivating case: the alternative is spelling `"app.storage.Todo"` out by hand, with nothing to check it.

An unresolvable type is `E0013`, exactly as in any other annotation. A **type parameter** resolves wherever the instantiation actually reaches the site — a top-level generic function's parameter and a generic type's parameter in an instance method both do — and is `E0058` where neither channel does; see below.

A generic type's parameter reaches an instance method off the **value's recorded instantiation**, so the value has to have been built at one the checker could see. Four positions supply it from an expected type (an annotated binding, a declared return, a field's declared type, a parameter's), and the call site can state it itself — `Repo::<Todo>.new("todos")`, see [Instantiating a generic *type* at the call site](Generics-and-Traits#instantiating-a-generic-type-at-the-call-site). A construction none of them reaches is `E0058` at the construction, not a clean check and a run-time abort at the first `type_name::<T>()`.

### Reflection over a type parameter

`field_specs_of::<T>()`, `variants_of::<T>()` and `construct::<T>(…)` are keyed on a type **name**, and for a statically written type that key is a compile-time constant — resolved like an annotation, folded, and never looked at again at run time.

A type **parameter** has no such constant: one compiled body serves every instantiation, and inside `fn f<T>()` the letter `T` is only ever the letter `T`. What the body does have is the instantiation's *name*, delivered per call — and a name is all these queries key on, so they take it:

```noeta
struct Todo {
    id: int
    title: string
}

fn count_of<T>(): int {
    return field_specs_of::<T>().len()
}

echo count_of::<Todo>()      // 2 — the real schema, per instantiation
```

This is exactly `field_specs_of(type_name::<T>())`, which has always worked and still does — the turbofish arm now composes it for you, through the same channel and the same helper `type_name::<T>()`, `v.as<T>()` and `v is T` read, so all of them agree about `T` by construction.

Two channels carry that name, and a surface reads whichever reaches it: a generic **type**'s parameter rides the receiver's recorded instantiation inside an instance method, and a generic **function's or method's own** parameter rides the hidden type-argument slot the call site fills. A **self-less** member of a generic type (a constructor, say — there is no receiver yet) takes the slot too, filled from the call's own instantiation.

`E0058` is what remains when *neither* channel reaches the body, and there the composed spelling fails for the same reason — so the help does not suggest it. The two cases are a nested `fn`'s own type parameter, which no call site instantiates, and a class's parameter inside a nested `fn`, which has no receiver to read the tag off. Reflect where the type is concrete and pass the result in — take a `List<FieldSpec>` (or a `string`) as a parameter and let the caller supply it.

```noeta error
class Repo<T> {
    pub tbl: string

    fn nested(): int {
        fn inner(): int { return field_specs_of::<T>().len() }   // E0058 — no receiver here
        if self.tbl == "" { return 0 }
        return inner()
    }
}
```

The **static** arm's other guarantee is unchanged: a turbofish naming a type that resolves to nothing is `E0013`, not a silent empty list. Leniency about an unrecognized *name* belongs to the runtime-string arm alone, where the name is data.

This holds for **every** name-keyed query, `attributes_of::<T>()` and `roles_of::<E>()` included, on **both** channels — they are one operand contract, resolved by one helper, so a capability a channel gains is a capability all of them gain:

```noeta
@attribute(Function)
@role(Semantic.EntryPoint)
struct Route { path: string }

#[Route("/users")]
fn handleUsers(): int { return 1 }

fn attrs_of<T>(): int { return attributes_of::<T>().len() }
fn roles_in<E>(): int { return roles_of::<E>().len() }

echo attrs_of::<Route>()        // 1
echo roles_in::<Semantic>()     // 1
```

The one surface that stays turbofish-only is **`from_bytes::<T>(blob)`**, and the reason is the operand rather than the machinery. The others key on a *name*, which is exactly what the two channels deliver; decoding an opaque byte buffer needs `T`'s packed **layout** — its field kinds and bit widths — and neither channel carries one. So a type parameter there is `E0058`, with a message naming the missing layout rather than blaming the element type.

### The prelude enums are ordinary enums

`Type` is one of five enums the language declares for you — `Ordering` (what `.compare()` returns), `Type`, `Semantic` (the built-in role vocabulary), `Layout` (the `@packed` storage vocabulary), and `Cancelled` (the `Err` payload of a cancelled `join`). Each is namable like any enum you declare yourself: you can annotate with it, `match` on it exhaustively, **and construct a case by name**. A constructed case is the very same value the runtime hands you, so `==` works and you are not forced into a `match` just to ask one question:

```noeta
echo type_of(5) == Type.Int                   // true
echo type_of([1]) == Type.List(Type.Int)      // true
echo 5.compare(2) == Ordering.Greater         // true
```

They are ordinary to **reflection** too: each answers [`variants_of`](#variants_oft-listvariantspec--variants_ofname-listvariantspec) with its cases and [`field_specs_of`](#field_specs_oft-listfieldspec--field_specs_ofname-listfieldspec) with the empty list — the pair that says "an enum, and here they are" rather than the both-empty pair that says "I have never heard of this name". That matters for the walk a schema-deriving framework actually performs, where the name it probes came from a `Type` value it was handed:

```noeta
echo variants_of("Ordering").map(fn(v) => v.name).join(" ")   // Less Equal Greater
echo field_specs_of("Ordering").len()                         // 0 — an enum declares no fields
```

The prelude **structs** are ordinary structs in the same way — `Attributed`, `RoleBinding`, `ParamInfo`, `FieldEntry`, `FieldSpec`, `VariantSpec`, `TierRoot`, `TierText` are constructible by literal, and a constructed one equals the materialized one field for field:

```noeta
struct P { a: int; b: string }

echo fields_of(P { a: 1, b: "x" })[0] == FieldEntry { name: "a", value: 1 }   // true
```

(`ParamInfo` and `FieldSpec` are the exception a literal cannot currently spell: their `type` field collides with the `type` keyword in struct-literal position. Reading `p.type` off one works — and `construct("FieldSpec", …)` builds one, since it keys on the schema rather than on the literal syntax.)

They reflect like ordinary structs too, which matters more than it sounds: `FieldSpec` and `VariantSpec` are the types you walk *while* reflecting, so a schema deriver that recurses into its own result type asks about them:

```noeta
for f in field_specs_of("FieldSpec") {
    echo "${f.name}: ${f.type}"
}
// name: Type.String
// type: Type.Named(Type, [])
// optional: Type.Bool
// attrs: Type.List(Type.Dyn)
```

Each shadows like any prelude name: declaring your own `enum Ordering` or `struct FieldEntry` replaces it for that program.

A **native** enum — one an extension registers, like `std.http`'s `Framing` — behaves the same, and does so under either spelling. A leaf import binds the short name; a group import lets you dot into the namespace, which is the spelling you need when two packages export the same short name:

```noeta
use std.http
use std.http.{Framing}

echo http.Framing.Sse == Framing.Sse   // true — one type, two spellings
```

It reflects the same way, and every static spelling keys on the one **qualified** identity — the name `type_of` reports for one of its values, and therefore the name a consumer that walked a `Type` will be holding:

```noeta
use std.http.{Framing}

echo variants_of::<Framing>().map(fn(v) => v.name).join(" ")   // Sse Ndjson Lines
echo type_name::<Framing>()                                    // std.http.Framing
```

A *dynamic* operand is still the literal string it spells, so `variants_of("Framing")` asks about the name `Framing` — reach for `type_name::<Framing>()`, or the name off a `Type`, when you need the key as data.

A native **fielded** type — a value struct or a class an extension declares — is reflectable the same way, under the same qualified identity, and its schema is the one `construct` accepts:

```noeta
use std.http.{Frame}

for f in field_specs_of::<Frame>() {
    echo "${f.name}: ${f.type} optional=${f.optional}"
}
// event: Type.String optional=false
// data: Type.String optional=false
// id: Type.String optional=false
// retry: Type.Option(Type.Int) optional=false
```

Every field of a native type is mandatory — the extension ABI gives a field no literal default — so a `construct` that omits one is refused (`missing required field …`) rather than silently filled. Supply the whole schema and you get the same value a literal builds:

```noeta
use std.http.{Frame}

fields: List<dyn> = ["msg", "hello", "id-1", some(7)];
same = match construct("std.http.Frame", fields) {
    Ok(v) => v.as<Frame>() == some(Frame { event: "msg", data: "hello", id: "id-1", retry: some(7) }),
    Err(e) => false,
};
echo same   // true
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

#### A native callable is indexed like any other

The signature index covers **every** callable the language knows, not only the ones written in `.noe`: the standard library's functions and its types' methods are in it too, under the same identity the rest of reflection uses — a module function's root-qualified path, a method's `Type.method` on the type's qualified identity:

```noeta
for p in params_of("std.math.pow") {
    echo "${p.name}: ${p.type}"          // base: Type.Float / exp: Type.Float
}
echo returns_of("std.math.pow")          // some(Type.Float)
echo returns_of("std.id.Uuid.to_string") // some(Type.String)
```

This is the `returns_of` contract taken seriously. `none` means *no callable of that name exists*, so a shipped stdlib function answering `none` reported a real function as a typo — the one thing the option was introduced to make detectable. Parameter names are the declared ones, so a container that injects by name works against a native signature exactly as against yours, and a declared native type is named by its qualified identity (`std.id.Uuid`) — the same string `type_of` reports for one of its values, so matching a signature against runtime values does not miss on the native half.

Two edges are worth knowing. A polymorphic native return is reported as precisely as the declaration allows and no further: `math.abs` returns `int` for an `int` and `float` otherwise, so it reflects `Type.Union([Type.Int, Type.Float])`, and a call-site-typed `json.try_parse::<T>` reflects its declared *wrapper* around a hole (`Type.Result(Type.Dyn, …)`), because `T` is named at the call site and a signature has no call site. And the target is the declaration's identity, not the spelling you call it by: after `use std.math`, you *write* `math.sqrt(2.0)`, but the callable is `std.math.sqrt` — a dynamic operand is the literal string it spells, the same rule `variants_of("Framing")` follows.

### `field_specs_of::<T>(): List<FieldSpec>` / `field_specs_of(name): List<FieldSpec>`

The **type-level** field schema of a declared struct or class — one `FieldSpec` per field in declaration order: `{ name: string, type: Type, optional: bool, attrs: List<dyn> }`. It is the declaration-side twin of `fields_of`: that one reflects an *instance*'s field **values** (and so sees the runtime-erased type), this one reflects the **declaration**, so `type` is precise, `optional` reports whether the field declared a default, and `attrs` holds the field's own `#[...]` attribute instances. An unknown name, or an enum, yields the empty list — an enum's cases are `variants_of`'s answer, and the two are meant to be asked as a pair.

Two surfaces, one node: the turbofish `field_specs_of::<T>()` when you know the type statically, and `field_specs_of(name)` when you hold it only as a runtime string (a `Type.Struct(name, _)` you just reflected). They converge on one name-keyed query, so both behave identically — including inside a module, where the turbofish resolves `T` to the same **qualified** identity `type_of` reports (`field_specs_of::<Todo>()` in the module `app.storage` asks for `app.storage.Todo`). The string surface takes the name verbatim, so it wants that qualified name too.

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

**A field describes itself, exactly as a parameter does.** `attrs` is the field half of `ParamInfo.attrs`, and it is there so the two doors are one walk: a library deriving a schema reads a callable's parameters with `params_of` and a type's fields with `field_specs_of`, and both hand back a descriptor carrying its own annotation, so one narrowing body serves both.

```noeta
@attribute(Param, Field)
struct Arg { help: string = "" }

struct Order {
    #[Arg(help: "the order id")] id: string
    qty: int = 1
}

fn help_of(attrs: List<dyn>): string {
    for a in attrs {
        if a is Arg { return a.help }
    }
    return ""
}

for spec in field_specs_of::<Order>() {
    echo "${spec.name}: ${help_of(spec.attrs)}"
}
// id: the order id
// qty:
```

Like the parameter half it is a **view** of the attribute manifest, not a second table: the instances are the same rows `attributes_of::<Arg>()` returns for the target `"Order.id"`, reached through the same key builder, so the two surfaces cannot disagree about which attribute belongs to which field. An unannotated field reports an empty list, never an absence — `qty` above has no `#[Arg]`, and `spec.attrs` is `[]` rather than missing.

### `variants_of::<T>(): List<VariantSpec>` / `variants_of(name): List<VariantSpec>`

The **type-level** variant schema of a declared enum — one `VariantSpec` per variant in declaration order: `{ name: string, payload: List<FieldSpec>, backing: ?dyn }`. It is the enum half of the same declaration-side query `field_specs_of` is the struct half of: two surfaces (turbofish and runtime string), one name-keyed query, the same **qualified**-identity resolution inside a module, and the same lenient answer for a name it does not recognize — a struct, a class, or an unknown name yields the empty list, so a framework can probe any name without a guard.

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

A variant's own `#[...]` attributes are deliberately **not** in the report. They are already keyed in the manifest under the qualified `Enum.Variant` target, the same `Type.field` convention the struct side uses, so `attributes_of::<T>()` is the one answer to "what is annotated on this variant". What the struct half carries (`FieldSpec.attrs`) is the *member-of-a-signature* case: a schema deriver walks parameters and fields side by side and needs those two descriptors to be the same shape, and a variant is not walked that way. A variant **payload** slot reports `attrs` as the empty list, which is the true answer rather than a stub — a payload slot has no attribute syntax to carry one.

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

Builds a struct, class, or **enum case** value **at runtime** from field values, through the *same* construction path a literal takes — so field defaults and full-initialization are honored identically, and a type that appears in no literal anywhere in the program still constructs. Like `field_specs_of` it has a turbofish and a runtime-string surface (with the same qualified-identity resolution inside a module); like `invoke` it is fallible by construction, returning a `Result` rather than aborting. Both surfaces are typed `Result<dyn, string>` — the turbofish only spells the type *name*, so narrow the `Ok` payload back with `.as<T>()` when you need the static type.

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
| a value supplied for a **private** field | ``` cannot set private field `secret` of `Box` from outside it ``` |
| the built value's own `validate()` rejected it | the validator's own message, e.g. ``` missing @: nope ``` |

Validation runs through one shared planner, so both backends accept, reject, and *word* every case identically.

The last two rows are the door refusing to mint a value the type's own declaration forbids — the sense in which `construct` really is the reflective form of a literal, rather than a way around one:

- **A private field cannot be set.** A `class`'s fields default private with a per-field `pub` opt-in, so `Box { secret: 9 }` written outside the class is `E0035`, and `construct("Box", {"secret": 9})` is that construction spelled reflectively. Only a *supplied* field is refused: omit a private field that has a default and it fills from that default, exactly as an outside-the-class literal which omits it does. A value `struct`'s fields are always public, so nothing about a struct is affected. The refusal is **context-free** — a runtime door knows neither its caller's type nor its tier, where the checker's gate relaxes inside the declaring type's own methods and inside a `@test` body — so it refuses there too; those are precisely the places where you can write the literal instead.
- **A `Validate` implementor's invariant runs.** See [`construct` enforces `impl Validate`](#construct-enforces-impl-validate).

#### `construct` is the reflective literal, not your constructor

Noeta has no constructor *declaration*. What people call one is a convention: a self-less method that returns its own type, spelled `new` because everyone spells it `new`. Nothing in the language privileges it, and `construct` does not know it exists.

So what `construct` is the reflective form *of* is the **`T { … }` literal**. It honors what the literal honors — field defaults, full-initialization, the declared field types — and it **bypasses everything `new` does**: normalization, invariants, derived fields, validation you wrote by hand.

```noeta
struct Slug {
    text: string
    normalized: bool = false

    // A constructor by convention only. `construct` has no idea this is here.
    pub fn new(text: string): Slug {
        return Slug { text: text.trim().lower(), normalized: true }
    }
}

s = Slug.new("  Hello World  ")
echo "new:       '${s.text}' normalized=${s.normalized}"
// new:       'hello world' normalized=true

mut raw: Map<string, dyn> = {}
raw["text"] = "  Hello World  "
echo match construct::<Slug>(raw) {
    // Untrimmed, unlowered, and `normalized` fell back to the FIELD's default —
    // not the `true` that `new` would have set.
    Ok(v)  => "construct: '${v.as<Slug>()?.text}' normalized=${v.as<Slug>()?.normalized}",
    Err(e) => e,
}
// construct: '  Hello World  ' normalized=false
```

This is the sharp edge in anything that builds user structs from **untrusted input** — CLI tokens, a model's JSON tool arguments, a request body. `construct` hands you a well-typed value that never passed through *your* hand-written normalization.

What it does **not** bypass is anything the type's own declaration states. A private field cannot be set through it, and an `impl Validate` invariant runs on the built value — see the two sections below. The line is: `construct` honors the declaration and skips the convention.

#### `construct` enforces `impl Validate`

If the constructed type implements [`Validate`](Validation), its `validate()` runs on the freshly built value and a rejection is the door's own `Err` carrying the validator's message — the same re-entry the `json` and `from_bytes` decode doors make. `construct` builds directly from untrusted data exactly as they do, so it enforces the invariant exactly as they do; a data door's exemption from the `@validated` construction ban (`E0060`) is earned by running the check, not granted by not being a literal.

The condition is **implementing `Validate`**, not carrying `@validated`. `@validated` decides where a *literal* may be written; the validator's presence decides whether a data door enforces it.

```noeta
struct Port {
    n: int

    impl Validate {
        pub fn validate(): Result<void, string> {
            if self.n < 1 || self.n > 65535 { return Err("port ${self.n} out of range") }
            return Ok()
        }
    }
}

echo match construct("Port", [70000]) {
    Ok(v)  => "built ${v}",
    Err(e) => "refused: ${e}",
}
// refused: port 70000 out of range
```

It is **bottom-up**, in the same sense the decode doors are, and for a simpler reason: `construct` never builds a nested value. Every field value you hand it is an existing value that already passed its own door, and the defaulted slots are filled before the type's own `validate` runs — so a container's validator only ever sees complete, already-valid fields, and an invalid inner is refused at its own `construct` call rather than surfacing as the container's complaint.

What is still yours to do is anything the type does not *declare*: normalization, derived fields, a `new` that means more than its fields.

Narrowing the `Ok` payload back to the static type is still yours — the door is typed `Result<dyn, string>` either way:

```noeta
struct Email {
    addr: string

    impl Validate {
        pub fn validate(): Result<void, string> {
            if !self.addr.contains("@") { return Err("missing @: ${self.addr}") }
            return Ok()
        }
    }
}

fn build(fields: Map<string, dyn>): Result<Email, string> {
    // The `?` carries the validator's rejection out — `construct` already ran it.
    v = construct::<Email>(fields)?
    match v.as<Email>() {
        some(e) => { return Ok(e) },
        none    => { return Err("not an Email") },
    }
}

mut bad: Map<string, dyn> = {}
bad["addr"] = "nope"
echo match build(bad) { Ok(e) => "ok ${e.addr}", Err(m) => "rejected: ${m}" }
// rejected: missing @: nope

mut good: Map<string, dyn> = {}
good["addr"] = "a@b.com"
echo match build(good) { Ok(e) => "ok ${e.addr}", Err(m) => "rejected: ${m}" }
// ok a@b.com
```

To run the *convention* rather than the declaration — a `new` that normalizes, derives, or means more than its fields — **call it by name.** If the type is statically known, `invoke(Slug, "new", args)` really does run `new`'s body — see [the receiver rules](#invokerecv-name-args-resultdyn-dyn--invokename-args-resultdyn-dyn). What you cannot do is get there from a *runtime string* name, which is exactly the gap `construct` fills and exactly why the gap has this shape.

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

### `attributes_of::<T>(): List<Attributed<T>>` / `attributes_of(name): List<Attributed<dyn>>`

Materializes every `#[T(...)]` attribute in the program — each entry's `.value` is a real `T`, and `.target` is the annotated declaration's name:

**"In the program" means every file the program is built from**, not only the declarations something imported. A data attribute is a **link root**: an annotated declaration in a sibling module, or in a dependency package, is part of the program whether or not any `use` names it — which is the whole point of tagging a function for discovery. So a `#[Tool]`-scanning framework finds the tools nothing statically references, and finds them by their **qualified** target name (`app.tools.run`, matching `type_of`'s naming inside a module). Visibility does not gate discovery either: a module-private `#[Tool] fn` is a registration, and `invoke(a.target, args)` really calls it — reflection and dispatch see the same set by construction. What the rule does *not* do is drag in unannotated code: a sibling's unannotated function that nothing imports stays out of the program, exactly as before.

```noeta
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

The **string arm** asks the same question with a key you are holding rather than one you can write: `attributes_of(name)` takes a runtime `string` and answers `List<Attributed<dyn>>`. The manifest is name-keyed either way, so the turbofish arm *is* the string arm with the name folded in. Only the turbofish arm is gated on `@attribute` — that gate needs a compile-time type to gate — and only it can be `E0013` for a name that resolves to nothing; the string arm is lenient like `field_specs_of(name)`, answering the empty list for a name the manifest holds nothing for.

### `roles_of(): List<RoleBinding>` / `roles_of::<RoleEnum>(): List<RoleBinding>` / `roles_of(name): List<RoleBinding>`

The compile-time `(declaration, role)` index built from `@role(...)` tags — each binding has a `.target` and a `.role`. The optional scope narrows the query to a single `@semantic` enum (the mirror of `attributes_of::<T>()`, and the same two operand arms): `roles_of::<Semantic>()` returns only the bindings whose role is a `Semantic` variant, `roles_of(name)` scopes by a name you are holding, and bare `roles_of()` returns the whole index. The turbofish arm is resolved at compile time (closed-world); naming a non-`@semantic` type there is an error (E0031), while the string arm is lenient and answers the empty list for an enum it knows nothing about.

It reads the same manifest `attributes_of` does, so it has the same reach: every annotated declaration in the program, including ones no `use` names, and including a role conferred by a *dependency package's* `@role`-bearing attribute. `.role` is a real enum value, so it compares directly — `if b.role == Semantic.TrustBoundary { … }`.

### `invoke(recv, name, args): Result<dyn, dyn>` / `invoke(name, args): Result<dyn, dyn>`

Fallible dispatch by name — and the one surface on this page that **consumes** a name rather than producing one. Reach for it when the callable's name arrives as *data*: a `#[Tool]` entry's `.target` off `attributes_of`, a router action, an argv subcommand. If you can write the call, write the call; `invoke` is not a faster way to do that, and there is deliberately no `invoke::<T>` turbofish arm to suggest otherwise.

With three operands, `recv` is a value (→ an instance method) or a bare type name (→ a static function):

```noeta
struct Rect {
    w: int
    h: int
    pub fn new(w: int, h: int): Rect { return Rect { w: w, h: h } }
    fn area(): int { return self.w * self.h }
}

echo match invoke(Rect.new(2, 3), "area", []) {
    Ok(v)  => "area = ${v}",             // area = 6
    Err(e) => "no such method",
}

// A bare TYPE in receiver position reaches a self-less static function — and
// really runs its body, normalization and all.
echo match invoke(Rect, "new", [2, 3]) {
    Ok(v)  => "made ${v.as<Rect>()?.w}x${v.as<Rect>()?.h}",   // made 2x3
    Err(e) => "${e}",
}
```

**The receiver is not a name — it is a value or a written type.** Both operands after it are runtime data, so it is easy to assume `recv` is too. It is not: handing it a `string` is a rejection, whatever that string spells.

```noeta
struct Rect {
    w: int
    h: int
    fn new(w: int, h: int): Rect { return Rect { w: w, h: h } }
}

name = "Rect"
echo match invoke(name, "new", [2, 3]) {
    Ok(v)  => "unreachable",
    Err(e) => "${e}",                    // cannot invoke on a value of type `string`
}
```

So there is no route from a *discovered* type name to that type's static functions — only from one you wrote. That is the gap [`construct`](#constructtfields-resultdyn-string--constructname-fields-resultdyn-string) exists to fill: it takes a runtime type name, at the price of [not being the constructor](#construct-is-the-reflective-literal-not-your-constructor).

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

### A declared conversion is named after its source

One method name is built rather than written: an [`impl From<Source>`](Error-Handling#converting-errors-at---impl-fromsource) conversion answers to **`from<Source>`**, not to `from`.

A conversion's identity is the pair of types it goes between, and a type may declare one per source, so `from` alone names a *set* — which is exactly the question a by-name lookup cannot answer. Asking for it is a miss, and the message names the alternatives:

```noeta
struct HttpError { status: int }
struct JsonError { line: int }

struct AppError {
    detail: string

    impl From<HttpError> {
        pub fn from(e: HttpError): AppError { return AppError { detail: "http" } }
    }

    impl From<JsonError> {
        pub fn from(e: JsonError): AppError { return AppError { detail: "json" } }
    }
}

echo invoke(AppError, "from", [HttpError { status: 404 }])
// Err(type `AppError` declares conversions from `HttpError` and `JsonError`; call `from<HttpError>` or `from<JsonError>`)

echo invoke(AppError, "from<HttpError>", [HttpError { status: 404 }])   // Ok(AppError {detail: "http"})
echo params_of("AppError.from<JsonError>").len()                        // 1
```

The source is spelled as the `impl` writes it, qualified where the type is (`from<std.json.JsonError>`); `type_name::<T>()` produces the same spelling, so a caller holding a type can build the name. Discovery and dispatch agree — `params_of("AppError.from")` describes nothing, because there is nothing there to call.

Writing the call directly needs none of this: `AppError.from(e)` picks the conversion by `e`'s type at compile time, and so does a `?`. The built name matters only where the name itself is data.

This is also why a **backed enum** keeps its built-in conversion after declaring one of its own — the two never contend for a slot:

```noeta
struct Raw { code: string }

enum Plan: string {
    Free = "free"
    Pro = "pro"

    impl From<Raw> {
        pub fn from(r: Raw): Plan { return Plan.Free }
    }
}

echo Plan.from("free")                  // Plan.Free — the backing value
echo Plan.from(Raw { code: "p" })       // Plan.Free — the declared conversion
```

## The manifest outside the language

The reflection manifest — declarations, their `#[…]` attributes, and their `@role`/`@semantic` tags — also backs the agentic tooling surface: `noeta mcp` serves it over stdio (roles, attributes, and the architectural graph) to MCP clients, so an agent asks the same index `roles_of()` and `attributes_of` answer in-language. See [Editor & AI Tooling](Editor-and-AI-Tooling) for the tool inventory.
