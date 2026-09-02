# Attributes & Reflection

The language distinguishes **codegen directives** (`@…`) from **data attributes** (`#[…]`), and exposes a runtime reflection surface.

> `@` = the compiler generates or registers something; `#[…]` = inert data metadata is attached.

## The decorator directives

Four `@` decorators attach metadata to a *declaration* or drive codegen on one: `@derive`, `@attribute`, `@role`, and `@semantic`. The layout directive `@packed` and the `@test`/`@bench`/`@doc`/`@debug` [dev-tier blocks](Dev-Tiers) use the same sigil for other jobs; see [Other `@` directives](#other--directives).

### `@derive(...)` — synthesize trait impls

Generates trait implementations from a type's shape. Covered in [Generics & Traits](Generics-and-Traits#derive--synthesized-implementations).

The **built-in recipe set** is closed: `Equatable`, `Comparable`, `Display`, `Error`, `Clone`, `Serialize<Format>`, `Deserialize<Format>`. **User traits are derivable too.** A fully-defaulted trait's defaults are adopted wholesale, as in the `Inspectable` example below, and a required method can be bridged onto a field or a method, or delegated `via:` a field. Every synthesized method is a mechanical bridge, forward, or default.

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

- Attributes are **structs**, because a struct has one canonical all-fields construction.
- Arguments map to fields, positional in declaration order or named. A field with a default is optional.
- Using an unmarked struct as an attribute is E0029. Writing `@attribute` itself on a class or enum is a misplaced directive, E0054.
- Placement can be constrained by listing target kinds, as `@attribute(Method, Function)` does, and a misplaced attribute is E0030. The kinds are `Struct`, `Class`, `Enum`, `Function`, `Method`, `Field`, `Variant`.
- Arguments are a **constant literal tree**: scalars, lists, maps, sets, enum values, nested struct literals, and a type reference, which becomes a reflection `Type` value. A non-literal argument such as `1 + 2` is E0003.
- A type-reference argument keeps its **generic arguments** at full fidelity, so `#[Builds(target: List<int>)]` reflects as `Type.List(Type.Int)`. It is validated exactly like a type annotation anywhere else: an unknown name inside it is E0013, and a built-in constructor applied at the wrong arity (`List<int, string>`) is E0058.

The `#[Skip]`, `#[Name]`, `#[Group]`, `#[Data]` and `#[Timeout]` attributes the [test runner](Testing) uses are such `@attribute` structs, as is [`#[Transient]`](Derives#keeping-a-field-out-of-the-wire), which takes a field out of its type's serialized shape.

Each lives in the module it belongs to rather than in the prelude. Bring one in with `use std.test.{Skip, Name, Group, Data, Timeout}` or `use std.json.Transient` at the top level, or qualify it inline as `#[std.test.Skip]`. Unimported, `#[Skip]` raises the ordinary "`Skip` cannot be used as an attribute" error. `#[Transient]` declares its placement as well, so writing it on anything but a field is E0030.

### `@role(Enum.Variant)` — a semantic role tag

A role tags a declaration with typed architectural meaning, "this is an entry point" or "this crosses a trust boundary", and tooling indexes it. That makes **architecture queryable**: `roles_of()` answers in-language, and `noeta mcp` serves the same index to AI agents (see [The manifest outside the language](#the-manifest-outside-the-language)).

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

Applying `@role` or `@semantic` to the wrong declaration kind is E0054, the one code every misplaced directive reports.

### Other `@` directives

Two more directive families use the `@` sigil for other jobs:

- **`@packed` / `@packed(Layout.Column)`**, a *layout* directive marking a struct as a packed value type, in flat or column-major storage. See [Fixed-Width Integers & Packed Types](Fixed-Width-Integers#packed-value-types--packed).
- **`@test` / `@bench` / `@doc` / `@debug`**, *dev-tier* blocks that gate co-located content. See [Dev Tiers](Dev-Tiers).

## The reflection surface

Prelude keywords expose type and metadata at runtime, with no import. The rest of this page documents each one.

### Choosing a surface

**Reach for a type test before you reach for reflection.** Where you hold a *value* and know the candidate types, `is` is the whole answer: it is checked at compile time, it **narrows** the scrutinee inside the arm, and a `match` over a union is exhaustive with no `_`. Reflection there costs you the checker.

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

`type_of` earns its place in that last arm, where there **is no candidate set**: the code must describe whatever arrives and cannot enumerate it in source. See [Type tests and narrowing](Type-System#type-tests-and-narrowing) for `is` and `.as<T>()` in full, and [Open vs. closed matching](Control-Flow-and-Pattern-Matching#open-vs-closed-matching) for why a union needs no `_` and `dyn` does.

The question you have picks the surface:

| The question | The surface | Notes |
|---|---|---|
| Is this value a `Todo`? (you can name the candidates) | `x is Todo` / `x.as<Todo>()` | Not reflection. Checked, and it narrows. |
| Does this value's type implement `Store`? | `x is dyn Store` | Same axis: one known trait, checked. |
| What type is this value, whatever it turns out to be? | [`type_of(x)`](#type_ofvalue-type) | The `Type` ADT, for a walk with no candidate set. |
| What is this type *called*, as a `string`? | [`type_name::<T>()`](#type_namet-string) when you can spell the type; [`t.name()`](#typename-string) on a `Type` you are already holding | Either way the qualified identity, the key every name-keyed query below is stored under. Never hand-write the string. |
| Which traits does this value's type implement, all of them? | [`traits_of(x)`](#traits_ofvalue-liststring--trait-membership-reflection) | A list of names, for a report. `is dyn Trait` for a decision. |
| What fields does this *value* carry, with their values? | [`fields_of(x)`](#fields_ofvalue--value-level-field-reflection) | Runtime-erased types, since it sees values. |
| What fields does this *type* declare, with their types and defaults? | [`field_specs_of`](#field_specs_oft-listfieldspec--field_specs_ofname-listfieldspec) | Declared types, precise. Ask with `variants_of`. |
| What cases does this enum declare? | [`variants_of`](#variants_oft-listvariantspec--variants_ofname-listvariantspec) | The enum half of the same query. |
| What does this callable take, and return? | [`params_of`](#params_ofname-listparaminfo) / [`returns_of`](#returns_ofname-type) | Declared types, with no value to test. |
| Which declarations carry `#[Route]`? | [`attributes_of::<Route>()`](#attributes_oft-listattributedt--attributes_ofname-listattributeddyn) | Whole-program, `use`-independent. |
| Which declarations carry an architectural role? | [`roles_of()`](#roles_of-listrolebinding--roles_ofroleenum-listrolebinding--roles_ofname-listrolebinding) | The compile-time `(declaration, role)` index. |
| Build a value of a type I hold only as a name | [`construct`](#constructtfields-resultdyn-string--constructname-fields-resultdyn-string) | The reflective **struct literal**, [not `new`](#construct-is-the-reflective-literal-not-your-constructor). |
| Call something whose name arrived as *data* | [`invoke`](#invokerecv-name-args-resultdyn-dyn--invokename-args-resultdyn-dyn) | The one consumer of the names the rest produce. |
| Rebuild a `List<T>` of a `@packed` type from a `bytes` blob | [`from_bytes::<T>(blob)`](Fixed-Width-Integers#bytes--serialize-a-packed-list) | A typed decode, not a query. Documented with packed types. |

#### The operand tells you which axis you are on

Every surface takes a **value**, a **type** (turbofish), or a runtime **string**, and the operands it accepts follow from what it answers:

| Operand | Surfaces | Why that shape |
|---|---|---|
| a **value** | `type_of`, `fields_of`, `traits_of` | You have the thing; the answer is about what it *is*. |
| a **type**, turbofish only | `type_name::<T>()`, `from_bytes::<T>(b)` | `type_name` has no string arm because it would be the identity function on its argument. `from_bytes` has none because it needs `T`'s packed *layout*, and a name does not carry one. |
| **either** | `field_specs_of`, `variants_of`, `construct`, `attributes_of`, `roles_of` | One name-keyed query, two ways to spell the key: a compile-time constant, or a name you are holding. |
| a runtime **string** only | `params_of`, `returns_of`, `invoke` | No turbofish arm: the target's name *arrives as data*. |

`params_of`, `returns_of` and `invoke` have no static arm, because their target arrives as data: a `#[Tool]` manifest entry's `.target`, a router action, an argv subcommand. **The other surfaces produce names; these three consume them.**

The middle row is one contract covering the whole name-keyed surface. Every query in it keys on a type *name*, so both arms end at the same runtime node. That is what lets the turbofish arm answer for a **type parameter**: generics are erased, and the instantiation's name reaches the body on a per-call channel, where the compiler routes the surface through its own string arm. See [Reflection over a type parameter](#reflection-over-a-type-parameter).

Reaching for the wrong axis is a **parse** error, `E0003`. `params_of::<Foo>()` fails at the operand, before typing runs, because a surface's operand shape is part of what it is.

### When the `Type` ADT earns its keep

`params_of` and `returns_of` are the ADT's dominant channel, outnumbering `type_of` by better than five to one across the `para/*` packages that consume reflection. They reflect *declared* types, where the value has not been built yet and there is nothing to `is`-test. A schema generator reads a signature and emits the JSON a caller must send, and a DI container reads a signature and decides what to inject. Both ask about a declaration.

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

Three properties of the ADT carry that walk, and it uses all three:

1. **Most types have no *key*.** A nominal, meaning a declared struct, class, or enum, is what has a name the name-keyed queries answer for. `Type.Union(members)`, `Type.Fn(params, ret)`, `Type.Option(inner)` and `Type.IntN(bits, signed)` have a head to print and nothing to look up, and their arguments are not in the head at all.
2. **The walk recurses, so `inner` must be a *value*.** `?List<string>` is two constructors deep before it reaches something a schema can name, and `render` gets `inner` handed to it and calls itself.
3. **Exhaustiveness is the totality proof.** `Type` is an ordinary enum, so a `match` over it with no `_` is checked (E0011). A schema-deriving walk therefore knows it is total, and every shape it cannot represent gets a deliberate error message. `para/ai`'s tool-schema walk is written this way, with all 23 arms and no `_`, and the unrepresentable ones return `Err` carrying a sentence about what to declare instead ("`bytes` has no tool-argument encoding — take a `string` and decode it in the tool").

Write such a walk with no `_` and let the checker list what you missed. There are 23 cases, `Type.Never` among them.

### `fields_of(value)` — value-level field reflection

`fields_of(value)` returns a struct or class instance's fields as `List<FieldEntry>`, each `{ name: string, value: dyn }`, in declaration order. Any other value yields the empty list. It is the value-level counterpart of `type_of`, and it lets a fully-defaulted trait implement *structural* behavior over `self` in pure Noeta, with no macro system:

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

**It reports the fields the call site could have read itself.** The door hands back *values*, so it answers the same visibility question a written `x.secret` does: outside the declaring type, a `class`'s private fields are absent. A `struct`'s fields are always public, so nothing about a struct is filtered.

Inside the type, `fields_of(self)` reports everything, as does `fields_of(other)` on a sibling instance, so a type can write about its whole shape. A trait's **default body** sees the public fields alone even when the implementor derives it, because that body is written in the trait and shared by every implementor, which puts it outside all of them. A walker taking `dyn` is outside every type it visits for the same reason:

```noeta
class Box {
    pub label: string
    secret: int

    pub fn new(l: string, s: int): Self { return Box { label: l, secret: s } }
    pub fn own(): dyn { return fields_of(self) }        // label and secret
}

b = Box.new("hi", 42)
fields_of(b)                                            // label only
```

A type that wants a private field on the wire says so with a derive. `Serialize` is written inside the declaration, so it speaks for the type. [`construct`](#constructtfields-resultdyn-string--constructname-fields-resultdyn-string) applies the same split from the other side, by refusing to *set* a private field. `field_specs_of` describes the declaration's shape and reads no values, so it lists every field.

### `traits_of(value): List<string>` — trait-membership reflection

`traits_of(value)` returns the trait names the value's nominal type has a **registered implementation** for, as a sorted, deduped `List<string>`. A registered implementation is a standalone `impl Trait for T`, an in-body `impl` block, a `@derive(Trait)`, or a native type's ABI-declared impl. It reads the same membership table the precise `x is dyn Trait` test consults, so the two agree by construction.

Names are **qualified**. A `.noe` trait keeps its linked name, bare for a trait in a package-less script and `shop.models.Trait` for one in a module, and a native trait reports its qualified identity, `std.vec.Kernels`. Built-in traits appear under their bare names, `Comparable` and `Display`, when a type registers an impl or derive of them.

Built-in *base* types (int, string, `List`, …) carry no declared impls, so `traits_of(42)` is `[]` while built-in protocol behavior such as `echo` and `<` still works on them. A non-nominal value, meaning a scalar, a collection or a function, yields the empty list, the same "nothing to report" answer `fields_of` gives a non-object.

```noeta
trait Speaks { fn speak(): string }

@derive(Comparable)
struct Dog { age: int }
impl Speaks for Dog { pub fn speak(): string { return "woof" } }

echo traits_of(Dog { age: 3 })   // ["Comparable", "Speaks"]
echo traits_of(42)               // []
```

### `type_of(value): Type`

Returns the value's runtime head-constructor as the prelude `Type` ADT, which you can `match`. Reach for it where the candidates cannot be named; where they can, [`x is T`](#choosing-a-surface) is shorter, checked, and narrows:

```noeta
echo match type_of(5) {
    Int    => "int",
    String => "string",
    _      => "other",
}
```

The payload-free cases are spelled **bare** here. The scrutinee is a `Type`, so `Int` and `String` resolve to that enum's own cases rather than binding the whole value (see [pattern matching](Control-Flow-and-Pattern-Matching#a-bare-identifier-is-a-variant-when-the-scrutinees-enum-has-one)). `Type.Int` means the same and reads better where the short name is ambiguous. The payload-carrying cases are call-shaped and need no qualifier: `List(inner)`, `IntN(bits, signed)`, `Struct(name, args)`.

`Type` has exactly **23** variants, and the whole list matters if you write a `_`-free walk over it: the scalars `Type.Int`, `Type.Float`, `Type.F32`, `Type.F64`, `Type.IntN(bits, signed)`, `Type.Bool`, `Type.String`, `Type.Bytes`, `Type.Unit`, `Type.Dyn`, `Type.Never`; the containers `Type.List(inner)`, `Type.Set(inner)`, `Type.Map(k, v)`, `Type.Option(inner)`, `Type.Result(ok, err)`; `Type.Fn(params, ret)` and `Type.Union(members)`; the trait object `Type.DynTrait(name)`; and the nominals `Type.Struct(name, args)`, `Type.Enum(name, args)`, `Type.Class(name, args)`, `Type.Named(name, args)`.

Collection literals carry their resolved element type as a runtime tag that survives a `dyn` launder. A content-changing op such as `.set` drops the tag to head-only.

`type_of` reports what a value **is**, so `Type.Never`, `Type.DynTrait` and `Type.Union` reach you through the *declaration* channel alone. `Type.Never` is uninhabited, so no value has it. A value behind a `dyn Trait` binding reports its own concrete type, leaving `Type.DynTrait` to the declaration side, and a value of a union type reports the member it holds, leaving `Type.Union` there too. All three are ordinary answers from `params_of` and `returns_of`:

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

A reflected type answers **its own head name**, the qualified and argument-free name of the type's head:

```noeta
struct Todo { id: int }

echo type_of(Todo { id: 1 }).name()       // Todo
echo type_of([1]).name()                  // List
echo type_of(5).name()                    // int
echo Type.DynTrait("Greet").name()        // Greet
```

It is the value-side counterpart of `type_name::<T>()` below, answering the **same qualified identity**, read off a `Type` you are *holding* rather than off a type you can spell. Inside a module `app.storage` that `Todo` reads `app.storage.Todo`, the name `type_of` shows inside `Type.Struct(name, args)`, verbatim and unshortened.

For a **nominal** that name is a *key*, the one the name-keyed queries (`field_specs_of(name)`, `variants_of(name)`, `construct(name, …)`) are stored under, so a walk can look up a type it merely encountered. For every other case it is a name to print: a `List` has no schema to fetch, and its element type is not in the head at all (see [When the `Type` ADT earns its keep](#when-the-type-adt-earns-its-keep)).

**Every case answers a non-empty name**, all 23 of them: a nominal its declared name, a container its constructor (`List`, `Map`, `Result`), a scalar its surface spelling (`int`, `f32`, `u8`), and the forms no bare name spells their constructor (`Fn`, `Union`, `unit`, `never`, `dyn`). A hand-written `match type_of(v) { Struct(n, _) => n, _ => "" }` answers the **empty string** for every case its match forgot, and that empty name travels on as a table name, a route, or a schema key.

It is spelled `name` to match the `name` field the other reflected descriptors carry (`FieldSpec.name`, `ParamInfo.name`, `VariantSpec.name`).

### `type_name::<T>(): string`

A type's **qualified runtime identity**, as a `string`. It is the name `type_of` reports inside `Type.Struct(name, args)`, and the key the name-keyed queries (`field_specs_of(name)`, `variants_of(name)`, `construct(name, …)`, `invoke(name, …)`) are stored under. It is how to *write* that key without spelling it out:

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

Turbofish only, since a `type_name(s)` taking a runtime string would be the identity function on `s`. The **compiler** resolves the type, so the name follows the module's path, a `use … as` alias, and a rename. A name-keyed repository is the motivating case: `Repository.new(type_name::<Todo>(), "todos", "id")` keeps the key checked where a hand-spelled `"app.storage.Todo"` has nothing checking it.

An unresolvable type is `E0013`, exactly as in any other annotation. A **type parameter** resolves wherever the instantiation reaches the site, which a top-level generic function's parameter and a generic type's parameter in an instance method both do. It is `E0058` where neither channel reaches; see below.

A generic type's parameter reaches an instance method off the **value's recorded instantiation**, so the value must have been built at one the checker could see. Four positions supply it from an expected type: an annotated binding, a declared return, a field's declared type, and a parameter's. The call site can also state it itself, `Repo::<Todo>.new("todos")`, per [Instantiating a generic *type* at the call site](Generics-and-Traits#instantiating-a-generic-type-at-the-call-site). A construction none of them reaches is `E0058` at the construction.

### Reflection over a type parameter

`field_specs_of::<T>()`, `variants_of::<T>()` and `construct::<T>(…)` are keyed on a type **name**. For a statically written type that key is a compile-time constant, resolved like an annotation and folded, with nothing left to look at at run time.

A type **parameter** has no such constant, since one compiled body serves every instantiation. What the body has is the instantiation's *name*, delivered per call, and a name is all these queries key on:

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

This is exactly `field_specs_of(type_name::<T>())`, which the turbofish arm composes for you, through the same channel `type_name::<T>()`, `v.as<T>()` and `v is T` read.

Two channels carry that name. A generic **type**'s parameter rides the receiver's recorded instantiation inside an instance method. A generic **function's or method's own** parameter rides the hidden type-argument slot the call site fills, and a **self-less** member of a generic type takes that slot too.

`E0058` is what remains when *neither* channel reaches the body. The two cases are a nested `fn`'s own type parameter, which no call site instantiates, and a class's parameter inside a nested `fn`, which has no receiver to read the tag off. Reflect where the type is concrete and pass the result in, taking a `List<FieldSpec>` or a `string` as a parameter.

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

The **static** arm's other guarantee holds here: a turbofish naming a type that resolves to nothing is `E0013`. Leniency about an unrecognized *name* belongs to the runtime-string arm, where the name is data.

This holds for **every** name-keyed query on **both** channels, `attributes_of::<T>()` and `roles_of::<E>()` included:

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

**`from_bytes::<T>(blob)`** stays turbofish-only, because of its operand. Decoding an opaque byte buffer needs `T`'s packed **layout**, meaning its field kinds and bit widths, which neither channel carries. A type parameter there is `E0058`, with a message naming the missing layout.

### The prelude enums are ordinary enums

`Type` is one of five enums the language declares for you: `Ordering` (what `.compare()` returns), `Type`, `Semantic` (the built-in role vocabulary), `Layout` (the `@packed` storage vocabulary), and `Cancelled` (the `Err` payload of a cancelled `join`).

Each is namable like any enum you declare yourself. You can annotate with it, `match` on it exhaustively, **and construct a case by name**. A constructed case is the same value the runtime hands you, so `==` answers a single question without a `match`:

```noeta
echo type_of(5) == Type.Int                   // true
echo type_of([1]) == Type.List(Type.Int)      // true
echo 5.compare(2) == Ordering.Greater         // true
```

They are ordinary to **reflection** too. Each answers [`variants_of`](#variants_oft-listvariantspec--variants_ofname-listvariantspec) with its cases and [`field_specs_of`](#field_specs_oft-listfieldspec--field_specs_ofname-listfieldspec) with the empty list, the pair that reads "an enum, and here they are" where both empty reads "nothing is known about this name". A schema-deriving framework performs exactly that walk, probing a name it took off a `Type` value it was handed:

```noeta
echo variants_of("Ordering").map(fn(v) => v.name).join(" ")   // Less Equal Greater
echo field_specs_of("Ordering").len()                         // 0 — an enum declares no fields
```

### The prelude structs are ordinary structs

`Attributed`, `RoleBinding`, `ParamInfo`, `FieldEntry`, `FieldSpec`, `VariantSpec`, `TierRoot` and `TierText` are constructible by literal, and a constructed one equals the materialized one field for field:

```noeta
struct P { a: int; b: string }

echo fields_of(P { a: 1, b: "x" })[0] == FieldEntry { name: "a", value: 1 }   // true
```

`ParamInfo` and `FieldSpec` are the exception a literal cannot spell, because their `type` field collides with the `type` keyword in struct-literal position. Reading `p.type` off one works, and `construct("FieldSpec", …)` builds one, since it keys on the schema rather than on the literal syntax.

They reflect like ordinary structs too. `FieldSpec` and `VariantSpec` are the types you walk *while* reflecting, so a schema deriver that recurses into its own result type asks about them:

```noeta
for f in field_specs_of("FieldSpec") {
    echo "${f.name}: ${f.type}"
}
// name: Type.String
// type: Type.Named(Type, [])
// optional: Type.Bool
// attrs: Type.List(Type.Dyn)
```

Every prelude name shadows. Declaring your own `enum Ordering` or `struct FieldEntry` replaces it for that program.

### Native types reflect the same way

A **native** enum, one an extension registers such as `std.http`'s `Framing`, behaves the same under either spelling. A leaf import binds the short name, and a group import lets you dot into the namespace, which is the spelling to reach for when two packages export the same short name:

```noeta
use std.http
use std.http.{Framing}

echo http.Framing.Sse == Framing.Sse   // true — one type, two spellings
```

It reflects the same way, and every static spelling keys on the one **qualified** identity, the name `type_of` reports for one of its values and therefore the name a consumer that walked a `Type` is holding:

```noeta
use std.http.{Framing}

echo variants_of::<Framing>().map(fn(v) => v.name).join(" ")   // Sse Ndjson Lines
echo type_name::<Framing>()                                    // std.http.Framing
```

A *dynamic* operand is the literal string it spells, so `variants_of("Framing")` asks about the name `Framing`. Reach for `type_name::<Framing>()`, or the name off a `Type`, when you need the key as data.

A native **fielded** type, a value struct or a class an extension declares, is reflectable the same way under the same qualified identity, and its schema is the one `construct` accepts:

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

Every field of a native type is mandatory, because the extension ABI gives a field no literal default, so a `construct` that omits one is refused with `missing required field …`. Supply the whole schema and you get the same value a literal builds:

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

Reflects a **callable's parameters** by name, one `ParamInfo` per parameter in declaration order. The name is a top-level function's bare name, or a method's qualified `Type.method`, the same target keying the attribute manifest.

| `ParamInfo` field | What it holds |
|---|---|
| `name: string` | the declared parameter name |
| `type: Type` | the parameter's *declared* type, in the same `Type` ADT `type_of` returns |
| `optional: bool` | whether a call may omit the parameter, meaning it declared a default |
| `attrs: List<dyn>` | the parameter's own `#[...]` attribute instances |


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

**Which names have a signature.** A `fn` declaration and a method always do. A top-level *binding* does when it is immutable and its initializer is a closure literal; see [Which names have a describable signature](#which-names-have-a-describable-signature). An unknown name answers with the empty list, the same answer a parameterless callable gives, so reach for `returns_of` and its `none` to tell a missing target from an empty one.

**Declared types and fixed widths.** `params_of` and `type_of` answer with the *same* `Type` for the same declared type, sharing one decoder.

A runtime scalar carries no width tag, so at **top level** a declared fixed-width scalar erases exactly as its value does: every `iN` and `uN` parameter reflects `Type.Int` and `f64` reflects `Type.Float`, while `f32` is reified and keeps `Type.F32`. In **container-element position** a width is a physically distinct storage slot and is preserved at any depth, so `List<i32>` reflects `Type.List(Type.IntN(32, true))`.

Matching a signature from `params_of` against runtime values, for dependency injection or for CLI and router derivation, therefore works for every scalar width: `type_of(5)` is `Type.Int`, and so is an `i32` parameter's `type`. See [Fixed-Width Integers](Fixed-Width-Integers) for the erasure model.

#### Which names have a describable signature

A description has to stay true for as long as the name exists, so `params_of` and `returns_of` cover only names whose signature cannot change: `fn` declarations, which are sealed, and **immutable** bindings of a closure literal. [`invoke`](#invokerecv-name-args-resultdyn-dyn--invokename-args-resultdyn-dyn) needs only the live value, so the set it can call is wider.

```noeta
fn declared(x: int): int { return x }

scale = fn(factor: int, by: int = 2) => factor * by   // immutable: described
mut hook = fn(x: int) => x + 1                        // reassignable: not described

echo params_of("declared").len()         // 1
echo params_of("scale").len()            // 2
echo params_of("hook").len()             // 0
```

A `mut` binding is excluded because a parameter *name* is not part of a function type. `mut hook = fn(a: int, b: int) => a - b` accepts `hook = fn(b: int, a: int) => a - b`, the same type with the names swapped, so a description taken from the initializer would name the wrong positions. Declare the binding without `mut` when you want it described.

A binding whose initializer is something other than a closure literal writes no parameter list at the binding site, so `alias = declared` and `made = build_handler()` have nothing there to describe. Describe `declared` under its own name.

### `returns_of(name): ?Type`

The other half of the same signature index: a callable's **declared return type**, keyed by exactly the string `params_of` takes, a bare fn name or a qualified `Type.method`. It makes a signature reflectable *end to end*, so a framework deriving an OpenAPI spec from controller methods reads the request shape out of `params_of` and the response shape out of `returns_of`.

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

An unannotated closure reports `Type.Dyn`, because its return type is inferred from its body rather than declared. A named `fn` must declare a return type, so an omitted one there means `void` and reports `some(Type.Unit)`.

The result is a `?Type`, and the option carries the difference between a missing target and a `void` one. `void` is a real answer, `some(Type.Unit)`, so a mistyped target answers `none` and a reflection-driven framework detects the route that would otherwise vanish. `params_of` has the empty list to spare for the same job, since an empty parameter list is a legitimate answer.

The `Type` comes out of the same decoder `ParamInfo.type` goes through, so a signature's parameters and its return agree about how a declared type spells, the kind-agnostic `Type.Named(name, [])` a declared struct, class or enum annotation reflects as included. A trait's abstract method signature is indexed too, under `Trait.method`. Reflection reports declared types throughout, so an `async fn f(): T` reports `T`, the type written in the declaration, where a call to it evaluates to a `Future<T>`.

#### A native callable is indexed like any other

The signature index covers **every** callable the language knows, the standard library's functions and its types' methods included, under the same identity the rest of reflection uses: a module function's root-qualified path, and a method's `Type.method` on the type's qualified identity.

```noeta
for p in params_of("std.math.pow") {
    echo "${p.name}: ${p.type}"          // base: Type.Float / exp: Type.Float
}
echo returns_of("std.math.pow")          // some(Type.Float)
echo returns_of("std.id.Uuid.to_string") // some(Type.String)
```

`none` means *no callable of that name exists*, and a shipped stdlib function is a callable, so it answers with its signature. Parameter names are the declared ones, so a container that injects by name works against a native signature exactly as against yours. A declared native type is named by its qualified identity, `std.id.Uuid`, the same string `type_of` reports for one of its values, so matching a signature against runtime values covers the native half.

A polymorphic native return is reported as precisely as the declaration allows. `math.abs` returns `int` for an `int` and `float` otherwise, so it reflects `Type.Union([Type.Int, Type.Float])`. A call-site-typed `json.try_parse::<T>` reflects its declared *wrapper* around a hole, `Type.Result(Type.Dyn, …)`, because `T` is named at the call site and a signature has no call site.

The target is the declaration's identity rather than the spelling you call it by. After `use std.math` you write `math.sqrt(2.0)`, and the callable is `std.math.sqrt`. A dynamic operand is the literal string it spells, the rule `variants_of("Framing")` follows.

### `field_specs_of::<T>(): List<FieldSpec>` / `field_specs_of(name): List<FieldSpec>`

The **type-level** field schema of a declared struct or class, one `FieldSpec` per field in declaration order.

| `FieldSpec` field | What it holds |
|---|---|
| `name: string` | the declared field name |
| `type: Type` | the field's *declared* type, precise |
| `optional: bool` | whether the field declared a default |
| `attrs: List<dyn>` | the field's own `#[...]` attribute instances |

It is the declaration-side twin of `fields_of`, which reflects an *instance*'s field **values** and so sees the runtime-erased type. An unknown name, or an enum, yields the empty list, since an enum's cases are `variants_of`'s answer and the two are asked as a pair.

Two surfaces reach one node. The turbofish `field_specs_of::<T>()` serves a type you know statically, and `field_specs_of(name)` serves one you hold as a runtime string, such as a `Type.Struct(name, _)` you just reflected. Both behave identically, inside a module included, where the turbofish resolves `T` to the same **qualified** identity `type_of` reports: `field_specs_of::<Todo>()` in the module `app.storage` asks for `app.storage.Todo`. The string surface takes the name verbatim and wants that qualified name too.

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

**A field describes itself, exactly as a parameter does.** `attrs` is the field half of `ParamInfo.attrs`, which makes the two doors one walk. A library deriving a schema reads a callable's parameters with `params_of` and a type's fields with `field_specs_of`, and both hand back a descriptor carrying its own annotation, so one narrowing body serves both.

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

Like the parameter half it is a **view** of the attribute manifest. The instances are the same rows `attributes_of::<Arg>()` returns for the target `"Order.id"`, reached through the same key builder, so the two surfaces agree about which attribute belongs to which field. An unannotated field reports an empty list, so `qty` above carries `spec.attrs` of `[]`.

### `variants_of::<T>(): List<VariantSpec>` / `variants_of(name): List<VariantSpec>`

The **type-level** variant schema of a declared enum, one `VariantSpec` per variant in declaration order.

| `VariantSpec` field | What it holds |
|---|---|
| `name: string` | the declared variant name |
| `payload: List<FieldSpec>` | the variant's payload, as declared-field data |
| `backing: ?dyn` | the variant's value in a backed enum |

It is the enum half of the declaration-side query `field_specs_of` is the struct half of, with the same two surfaces, the same **qualified**-identity resolution inside a module, and the same lenient answer for an unrecognized name. A struct, a class, or an unknown name yields the empty list, so a framework can probe any name without a guard.

Ask them **together**. `field_specs_of` answers an enum with the empty list and a field-less struct with the empty list too, so through that query alone an enum reads as an empty struct. With the pair, fields present means a struct or class, variants present means an enum, and both empty means nothing is known about the name:

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

A variant's **payload** is reported as ordinary declared-field data, through the same `FieldSpec` the struct side uses. A positional payload carries a synthesized `_0` or `_1` name with its real declared type in the type slot, so a positional and a named payload read alike at the consumer. `optional` is always `false`, since a variant payload field has no syntax for a default.

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

`backing` is the variant's value in a **backed enum** (`enum Status: string`), as `some(value)`, which is the wire value a derived schema emits in place of the variant name. A plain enum's variants report `none`, so the `?` distinguishes "backed by this" from "backed by nothing":

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

A variant's own `#[...]` attributes live in the manifest alone, keyed under the qualified `Enum.Variant` target, the same `Type.field` convention the struct side uses, so `attributes_of::<T>()` is the one answer to "what is annotated on this variant". A variant **payload** slot reports `attrs` as the empty list, since a payload slot has no attribute syntax to carry one.

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

Builds a struct, class, or **enum case** value **at runtime** from field values, through the *same* construction path a literal takes. Field defaults and full-initialization are honored identically, and a type that appears in no literal anywhere in the program still constructs.

Like `field_specs_of` it has a turbofish and a runtime-string surface, with the same qualified-identity resolution inside a module. Like `invoke` it is fallible by construction, returning a `Result`. Both surfaces are typed `Result<dyn, string>`, since the turbofish spells the type *name* alone, so narrow the `Ok` payload back with `.as<T>()` when you need the static type.

`fields` accepts either shape:

- a **`List<dyn>`**, positional and in declaration order. A list shorter than the field count fills the remaining fields from their defaults, so trailing optional fields may be left off. It expresses no *gap*, meaning an omitted middle field.
- a **`Map<string, dyn>`**, named, sparse and in any order. This is the form a CLI expanding a struct into `--field` flags produces: supply `port` and `verbose` and let the middle `host` fall back to its default.

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

Every rejection is an `Err(string)` carrying a ready-to-surface message, and nothing in the table aborts:

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

The last two rows are the door holding a value to the type's own declaration, which is the sense in which `construct` is the reflective form of a literal.

**A `Validate` implementor's invariant runs.** See [`construct` enforces `impl Validate`](#construct-enforces-impl-validate).

#### A private field cannot be set through `construct`

A `class`'s fields default private with a per-field `pub` opt-in, so `Box { secret: 9 }` written outside the class is `E0035`, and `construct("Box", {"secret": 9})` is that construction spelled reflectively. A *supplied* field is what gets refused, so omitting a private field that has a default fills it from that default, exactly as an outside-the-class literal omitting it does. A value `struct`'s fields are always public, so a struct is unaffected.

The refusal is **context-free**. A runtime door knows neither its caller's type nor its tier, so it refuses inside the declaring type's own methods and inside a `@test` body as well, where the checker's gate relaxes. Those are the places where you can write the literal instead.

#### `construct` is the reflective literal, not your constructor

Noeta has no constructor *declaration*. What people call one is a convention: a self-less method that returns its own type, spelled `new`. The language privileges it nowhere, and `construct` does not know it exists.

`construct` is the reflective form of the **`T { … }` literal**. It honors what the literal honors, meaning field defaults, full-initialization and the declared field types, and it **bypasses everything `new` does**: normalization, invariants, derived fields, and validation you wrote by hand.

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

This is the sharp edge in anything that builds user structs from **untrusted input**: CLI tokens, a model's JSON tool arguments, a request body. `construct` hands you a well-typed value that skipped *your* hand-written normalization.

**`construct` honors the declaration and skips the convention.** A private field cannot be set through it, and an `impl Validate` invariant runs on the built value; see the two sections below.

#### `construct` enforces `impl Validate`

If the constructed type implements [`Validate`](Validation), its `validate()` runs on the freshly built value, and a rejection is the door's own `Err` carrying the validator's message. The `json` and `from_bytes` decode doors make the same re-entry. `construct` builds directly from untrusted data as they do, so it enforces the invariant as they do, and a data door earns its exemption from the `@validated` construction ban (`E0060`) by running the check.

The condition is **implementing `Validate`**. `@validated` decides where a *literal* may be written, and the validator's presence decides whether a data door enforces it.

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

It is **bottom-up**, as the decode doors are, and for a simpler reason: `construct` builds no nested value. Every field value you hand it is an existing value that already passed its own door, and the defaulted slots are filled before the type's own `validate` runs. A container's validator therefore sees complete, already-valid fields, and an invalid inner is refused at its own `construct` call.

Anything the type leaves undeclared is yours to do: normalization, derived fields, a `new` that means more than its fields.

Narrowing the `Ok` payload back to the static type is yours too, since the door is typed `Result<dyn, string>` either way:

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

To run the *convention*, a `new` that normalizes, derives, or means more than its fields, **call it by name.** Where the type is statically known, `invoke(Slug, "new", args)` runs `new`'s body; see [the receiver rules](#invokerecv-name-args-resultdyn-dyn--invokename-args-resultdyn-dyn). A *runtime string* name reaches `construct` and nothing else, which is the gap `construct` fills.

#### Constructing an enum case

An enum case is spelled `construct("Enum.Variant", payload)`. The case goes where the type name goes, exactly as it is written in source, and `fields` is that variant's **payload**.

```noeta
enum Shape { Circle(r: int); Rect(w: int, h: int); Dot }

echo match construct("Shape.Rect", [2, 5]) {
    Ok(v) => "${v}",                    // Shape.Rect(2, 5)
    Err(e) => e,
}
```

The payload takes the same two shapes a struct's fields do, and means the same things: a positional `List<dyn>` in declaration order, or a `Map<string, dyn>` keyed by the payload's field names. Those names are the ones [`variants_of`](#variants_oft-listvariantspec--variants_ofname-listvariantspec) reports, a named payload's declared names or the synthesized `_0` and `_1` of a positional one, so the query that tells you what a case needs and the call that builds it speak one vocabulary:

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

A **bare enum name** is a rejection, and the message teaches the `Enum.Variant` spelling. The turbofish reports the same error, since `construct::<Shape>(…)` spells a type *name* alone.

To go the other way, from a single **wire value** to a case, use [`Enum.try_from`](Structs-Classes-and-Enums#enums), which matches a backed enum's backing.

### `attributes_of::<T>(): List<Attributed<T>>` / `attributes_of(name): List<Attributed<dyn>>`

Materializes every `#[T(...)]` attribute in the program. Each entry's `.value` is a real `T`, and `.target` is the annotated declaration's name.

**"In the program" means every file the program is built from.** A data attribute is a **link root**: an annotated declaration in a sibling module, or in a dependency package, is part of the program whether or not any `use` names it, which is what tagging a function for discovery is for. A `#[Tool]`-scanning framework therefore finds the tools nothing statically references, by their **qualified** target name (`app.tools.run`, matching `type_of`'s naming inside a module).

Visibility leaves discovery alone. A module-private `#[Tool] fn` is a registration, and `invoke(a.target, args)` calls it, so reflection and dispatch see one set. The link root is the *annotation*, so a sibling's unannotated function that nothing imports stays out of the program.

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

A type-reference argument arrives as a full reflection `Type` value with its generic arguments intact, which is what a codegen or DI consumer needs to reconstruct the declared type:

```noeta
@attribute
struct Builds { target: Type }

#[Builds(target: List<int>)]
fn make_list(): List<int> { return [] }

for b in attributes_of::<Builds>() {
    echo "${b.target}: ${b.value.target}"    // make_list: Type.List(Type.Int)
}
```

The **string arm** asks the same question with a key you are holding. `attributes_of(name)` takes a runtime `string` and answers `List<Attributed<dyn>>`. The manifest is name-keyed either way, so the turbofish arm *is* the string arm with the name folded in.

The turbofish arm alone is gated on `@attribute`, which needs a compile-time type to gate, and it alone can be `E0013` for a name that resolves to nothing. The string arm is lenient like `field_specs_of(name)`, answering the empty list for a name the manifest holds nothing for.

### `roles_of(): List<RoleBinding>` / `roles_of::<RoleEnum>(): List<RoleBinding>` / `roles_of(name): List<RoleBinding>`

The compile-time `(declaration, role)` index built from `@role(...)` tags. Each binding has a `.target` and a `.role`.

The optional scope narrows the query to a single `@semantic` enum, mirroring `attributes_of::<T>()` across the same two operand arms. `roles_of::<Semantic>()` returns the bindings whose role is a `Semantic` variant, `roles_of(name)` scopes by a name you are holding, and bare `roles_of()` returns the whole index. The turbofish arm is resolved at compile time against a closed world, and naming a non-`@semantic` type there is E0031. The string arm is lenient and answers the empty list for an enum it knows nothing about.

It reads the same manifest `attributes_of` does, so it has the same reach: every annotated declaration in the program, ones no `use` names included, and a role conferred by a *dependency package's* `@role`-bearing attribute. `.role` is a real enum value, so it compares directly: `if b.role == Semantic.TrustBoundary { … }`.

### `invoke(recv, name, args): Result<dyn, dyn>` / `invoke(name, args): Result<dyn, dyn>`

Fallible dispatch by name, and the one surface on this page that **consumes** a name rather than producing one. Reach for it when the callable's name arrives as *data*: a `#[Tool]` entry's `.target` off `attributes_of`, a router action, an argv subcommand. Where you can write the call, write the call; there is no `invoke::<T>` turbofish arm.

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

**The receiver is a value or a written type.** Both operands after it are runtime data, and the receiver is not one: handing it a `string` is a rejection, whatever that string spells.

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

A type's static functions are therefore reachable from a type name you wrote, and from a *discovered* one they are not. [`construct`](#constructtfields-resultdyn-string--constructname-fields-resultdyn-string) fills that gap by taking a runtime type name, at the price of [being the reflective literal](#construct-is-the-reflective-literal-not-your-constructor) rather than the constructor.

With two operands, `name` is a **top-level binding that holds a callable**, a `fn` declaration or a closure-valued binding alike, since dispatching needs the value the name holds:

```noeta
fn greet(who: string = "stranger"): string { return "hi ${who}" }

mut shout = fn(who: string) => "HI ${who.upper()}"

echo match invoke("greet", ["ada"]) {
    Ok(v)  => v,                         // hi ada
    Err(e) => "no such function",
}

echo match invoke("shout", ["ada"]) {
    Ok(v)  => v,                         // HI ADA
    Err(e) => "no such function",
}
```

The two-operand form searches the top-level namespace. A type name, a qualified `Type.method`, and a local variable holding a function are each a miss. The three-operand form reaches a type's methods, and a function value you already hold you can call directly.

#### Passing `args` positionally or by name

`args` accepts either shape, exactly as `construct`'s `fields` does, and in both the two- and three-operand forms:

- a **`List<dyn>`**, positional and in declaration order. A list shorter than the parameter count leaves the remaining parameters to their defaults, so trailing optional parameters may be left off. It expresses no *gap*.
- a **`Map<string, dyn>`**, named, sparse and in any order. This is the form a caller filling a signature from `params_of` produces: supply `a` and `c` and let the middle `b` run its default.

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

The omitted `qty` runs its compiled default expression, exactly as at a direct `place(item: "widget", note: "rush")` call site, since the named form is the same calling convention reached by name. Parameter names come from the same signature index `params_of` reads, so reflecting a signature and then calling it by name round-trips on one target string.

The named form therefore reaches only the names that index [describes](#which-names-have-a-describable-signature), which is narrower than the set `invoke` can call. The positional form is unaffected.

Every rejection is an `Err` and nothing here aborts: an unknown name, a non-string name, args that are neither a list nor a map, an arity mismatch, and, in the named form:

| Situation | Message |
|---|---|
| `args` is neither a list nor a map | ``invoke args must be a list or a map, found <kind>`` |
| a named argument the callable has no parameter for | ``` `place` has no parameter `nope` ``` |
| a parameter that is neither supplied nor defaulted | ``` missing required parameter `item` of `place` ``` |
| a callable the signature index does not describe (a global holding a closure that was never declared as a `fn`) | ``` `f` does not take named arguments ``` |

The named form leaves an argument's type to the callee's own typing, as the positional form does, so one call answers alike through both. `construct` differs there, checking a value's scalar kind against the declared field type.

A parameter with a default may be omitted from either shape, exactly as at a direct call site, which is the pair to `ParamInfo.optional`. The by-name resolution is what is caught, so a panic *inside* the invoked body is a normal abort.

### A declared conversion is named after its counterpart

One method name is built rather than written. An [`impl From<Source>`](Error-Handling#converting-errors-at---impl-fromsource) conversion answers to **`from<Source>`**, and an [`impl To<Target>`](Error-Handling#converting-into-a-type-you-do-not-own--impl-totarget) answers to **`to<Target>`**.

A conversion's identity is the pair of types it goes between, and a type may declare one per counterpart, so the bare name names a *set* and a by-name lookup has nothing to pick from it. Asking for the bare `from` is a miss, and the message names the alternatives:

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

The source is spelled as the `impl` writes it, qualified where the type is (`from<std.json.JsonError>`), and `type_name::<T>()` produces the same spelling, so a caller holding a type can build the name. Discovery and dispatch agree: `params_of("AppError.from")` describes nothing, because there is nothing there to call.

A directly written call needs none of this. `AppError.from(e)` picks the conversion by `e`'s type at compile time, and so does a `?`. The built name matters where the name itself is data.

A **backed enum** therefore keeps its built-in conversion alongside one of its own, since the two occupy different slots:

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

The reflection manifest holds declarations, their `#[…]` attributes, and their `@role` and `@semantic` tags, and it backs the agentic tooling surface as well. `noeta mcp` serves it over stdio to MCP clients, roles and attributes and the architectural graph alike, so an agent asks the same index `roles_of()` and `attributes_of` answer in-language. See [Editor & AI Tooling](Editor-and-AI-Tooling) for the tool inventory.
