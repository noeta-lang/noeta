# Derives — @derive

`@derive(...)` generates trait impls from a type's shape. It is a *codegen* directive, distinct from `#[...]` data attributes (see [Attributes & Reflection](Attributes-and-Reflection)). Everything a derive synthesizes obeys the ordinary [coherence rule](Generics-and-Traits#coherence): at most one implementation of a trait per type, however it got there.

## The built-in recipes

A bare `@derive(Name)` on one of these synthesizes the implementation from the type's shape. The set is closed, and a built-in outside the table carries no recipe, so deriving it is E0014. Several of those are still reachable through [`via:`](#delegating-through-a-field-via), which delegates a trait through a field.

| Built-in | What the derive gives the type |
|---|---|
| `Equatable` | Structural equality, as the `==` answer and as a callable `eq(other): bool`. |
| `Comparable` | Field-wise **structural** ordering, in declaration order, recursing into nested objects and enum payloads and comparing each structurally in turn. On an **enum**: variant declaration order first (`Low < Medium < High`), then payload fields. It is what `< <= > >=`, `.sorted()`, `.min()`/`.max()` and the callable `compare(other): Ordering` all read. |
| `Display` | `to_string(): string` returning the structural rendering, character for character the text `echo` and `${…}` write for the value. A competing hand-written `impl Display` is a coherence error. |
| `Error` | `message()` returns `"${self}"`, the type's display story: a hand-written `impl Display`'s `to_string()`, or the structural rendering under `@derive(Display)`. It requires the type to have `Display` at all (E0050 otherwise); `@derive(Error, via: field)` instead forwards `message()` into the field's own `Error` implementation. See [Error Handling](Error-Handling#deriving-error). |
| `Clone` | Membership alone: the type satisfies a `<T: Clone>` bound and reports `Clone` from `traits_of`. There is no method behind it and no `dyn Clone`, because a `struct` is a value type and copies on assignment, so a copy needs no call. |
| `Serialize<Json>` | `to_json()`, encoding by the rules [below](#what-serializejson-writes). On an enum it writes the variant rendering `json.stringify` produces. |
| `Deserialize<Json>` | The type's decode recipe, so JSON decodes into it: `json.parse::<T>` / `json.try_parse::<T>`, `Response.json::<T>()`, and the router-facing `json.decode_typed("T", text)`, which resolves the type by *name* at runtime and needs this derive. Derivable for a non-generic `struct` or `class` whose fields are all JSON-decodable: numbers (including the [fixed widths](Fixed-Width-Integers), which accept a JSON number that fits the declared width and report one that does not), `bool`, `string`, `?T`, `List`, string-keyed `Map`, a declared **enum**, or another such type. Anything else is E0050. A decoded `class` is a class: it has identity, compares by reference, and runs its `Validate` at the door like any other decode. See [which fields may be omitted](#which-json-fields-may-be-omitted) and [enum-typed fields](#enum-typed-fields) below. |

Deriving `Comparable` is how a type opts into being ordered at all. A type wanting a *different* order writes its own `compare` instead ([Ordering your own type](Generics-and-Traits#ordering-your-own-type)), and it does one or the other, since a type implements each trait once.

`Display`, `Equatable` and `Comparable` each make their method **callable by name**: `x.to_string()`, `x.eq(other)`, `x.compare(other)`. The method runs the same routine the operator runs, so a derived `compare` and `<` read one ordering and a derived `to_string()` is exactly what interpolation writes. It answers through the [trait object](Generics-and-Traits#trait-objects) too, so a value that reports `Display` from `traits_of` can always be handed to a `dyn Display` and asked.

```noeta check
@derive(Equatable, Comparable, Display, Clone)
class Point {
    x: int
    y: int
    pub fn new(x: int, y: int): Point { return Point { x: x, y: y } }
}
echo Point.new(1, 2) < Point.new(1, 3)          // true
echo Point.new(1, 2).compare(Point.new(1, 3))   // Ordering.Less
echo Point.new(1, 2).to_string()                // Point {x: 1, y: 2}

@derive(Serialize<Json>)
class User {
    name: string
    id: int
    active: bool
    pub fn new(name: string, id: int, active: bool): User { return User { name: name, id: id, active: active } }
}
echo User.new("Ada", 7, true).to_json()  // {"name":"Ada","id":7,"active":true}
```

### What `Serialize<Json>` writes

- **Every field** except one marked [`#[Transient]`](#keeping-a-field-out-of-the-wire). A default is a decode-side notion, so encoding never omits a field for having one.
- A `bytes` field as a JSON **array of its byte values** (`[104, 105]`), the lossless spelling that needs no agreed side-channel like base64. It is write-only, since `bytes` is not a decodable field type.
- An **enum value** as its case name when the variant is payload-free (`"Green"`), and as `{"Case":[payload…]}` when it carries one, so `Shape.Circle(3)` writes `{"Circle":[3]}` and the payload travels positionally. `Result` is a plain enum, so `Ok(5)` is `{"Ok":[5]}`. The payload-carrying form is write-only, since [a payload-carrying enum has no JSON decoding at all](#enum-typed-fields).
- An `Option` by the JSON-null convention, which is the one exception: `some(x)` encodes as `x` and `none` as `null`, at the top level and inside a variant's payload alike.

## Which JSON fields may be omitted

`@derive(Deserialize<Json>)` reads optionality off the declaration, and the rule is exact:

| Field declaration | An input that omits it |
|---|---|
| `name: ?T` | decodes to `none` |
| `name: T = <literal>` | decodes to the declared default |
| `name: T = <anything else>` (`= now()`, `= helper()`) | **required** — the error says the default is not a literal |
| `name: T` | **required** — the missing-field error, naming the field and the type |

A default makes the field *optional*, not untyped: a value that **is** present still has to match the field's type, and it always wins over the default.

```noeta
use std.json

@derive(Deserialize<Json>)
struct Pet {
    id: int
    name: string = "(unnamed)"
}

pet = json.parse::<Pet>("{\"id\": 7}")
echo "${pet.id}: ${pet.name}"      // 7: (unnamed)
```

A decode is a pure data walk with no access to the running program, so it can only fill a default it carries *as data*, which is where the literal cutoff comes from. Every in-process constructor, meaning a `T { … }` literal or `construct(name, fields)`, runs the field's compiled default expression instead and therefore accepts a non-literal default. `field_specs_of::<T>()` reports `optional = true` for any default at all; a decode narrows that to the literal ones and tells you when it did.

The same rule governs request bodies. A web framework decodes a handler's body parameter with `json.decode_typed`, so a body struct with a literal default accepts documents that omit that field.

## A derive sees the whole type

`Serialize` writes every field and `Deserialize` reads every field, **private ones included**. One field at a time can be excluded by the declaration itself: see [`#[Transient]`](#keeping-a-field-out-of-the-wire).

A derive is written *inside* the declaration, next to the methods, so it is the type saying what its wire form is. A caller-side reflective door has no such standing: `construct` refuses to set a private field by name, and `fields_of` reports only the fields you could have read yourself, because those are reached from outside by code the type never authorized. **`pub` governs access, a derive governs shape.**

Encoding writes every field, so a wire form of the whole value exists whether or not anything can read it back. A decode that skipped private fields would build a value the encoder never described.

```noeta
use std.json

@derive(Serialize<Json>, Deserialize<Json>)
class Box {
    pub label: string
    secret: int                       // private, and still part of the wire form

    pub fn new(l: string, s: int): Self { return Box { label: l, secret: s } }
    pub fn peek(): int { return self.secret }
}

wire = json.stringify(Box.new("hi", 42))    // {"label":"hi","secret":42}
back = json.parse::<Box>(wire)
echo back.peek()                            // 42
```

## Keeping a field out of the wire

`#[Transient]` takes one field out of the type's serialized shape: the encoder never writes it, and the decoder never reads it, so a document carrying the key is not consulted for it either. Everything else about the field is unchanged, and it is constructed, read, mutated and compared like any other.

```noeta
use std.json
use std.json.Transient

@derive(Serialize<Json>, Deserialize<Json>)
class Basket {
    pub id: string
    #[Transient] pub mut hits: int = 0    // a live counter, meaningless to anyone else
}

json.stringify(Basket { id: "abc", hits: 7 })     // {"id":"abc"} — no `hits`
json.parse::<Basket>("{\"id\":\"abc\"}").hits     // 0, from the declaration
```

**A transient field must be able to supply its own value**, since the wire will not: give it a default the compiler can fold to a literal, or make it optional (`?T`, which fills with `none`). A field that can do neither is refused where the derive is written, rather than at the first parse. A `Serialize`-only type is unconstrained, because nothing ever fills anything.

The marker is also what makes a type with an unserializable field serializable at all. `@derive(Serialize<Json>)` is refused when a field has no JSON form, and that verdict is about the whole type; marking the field takes it out of the shape, so the rest travels:

```noeta
use std.json.Transient

@derive(Serialize<Json>)
class Pool {
    pub host: string
    #[Transient] pub open: Set<string> = #{}    // a `Set` has no JSON form — and does not need one
}
```

It governs every boundary the value crosses out of the program, not JSON alone: the same field is absent from a native function's arguments, a database bind, and an isolate's output. A field holding something with no sense outside this process has nothing to send anywhere.

## Enum-typed fields

An enum-typed field decodes from the wire values the enum's own JSON Schema advertises, and only those, so that the schema and the decoder describe one vocabulary.

| Enum | Its schema | What decodes |
|---|---|---|
| `enum Mood { Positive; Negative }` | `{"enum": ["Positive", "Negative"]}` | the **case names** |
| `enum Plan: string { Free = "free"; Paid = "paid" }` | `{"enum": ["free", "paid"]}` | the **backings** — `"Free"` is refused |
| `enum Code: int { Ok = 200 }` | `{"enum": [200]}` | the **backings**, as JSON numbers |

```noeta
use std.json
use std.json.JsonError

enum Plan: string { Free = "free"; Paid = "paid" }

@derive(Deserialize<Json>)
struct Account { tier: Plan }

fn describe(r: Result<Account, JsonError>): string {
    return match r {
        Ok(a) => "${a.tier}",
        Err(e) => "${e.kind()}: ${e.message()}",
    }
}

echo describe(json.try_parse::<Account>("{\"tier\": \"paid\"}"))   // Plan.Paid
echo describe(json.try_parse::<Account>("{\"tier\": \"gold\"}"))
// unknown_variant: tier: "gold" is not a variant of `Plan`: expected one of "free", "paid"
```

What you get back is a **real enum value**: it `match`es exhaustively and compares equal to a case written in source.

A value that names no case is a path-carrying `JsonError` of kind `unknown_variant`, whose detail lists every accepted wire value. That is distinct from `mismatch`, which means the document has the wrong *shape*. Both are ordinary errors, so neither can panic and neither can produce a silently-wrong value.

An enum with a **payload-carrying** variant has no JSON decoding at all, so a struct with such a field is E0050: a data-carrying sum has no canonical JSON spelling, and decoding only its payload-free half would accept documents against a schema that cannot describe the type. Build those cases with [`construct("Enum.Variant", payload)`](Attributes-and-Reflection#constructtfields-resultdyn-string--constructname-fields-resultdyn-string), or convert a single wire value with [`Enum.try_from`](Structs-Classes-and-Enums#enums).

Encoding is therefore **asymmetric** for such a variant. `json.stringify` and `to_json()` write it as `{"Case":[payload…]}` so nothing is lost on the way out, and no decode accepts that form on the way back.

## Deriving a user trait

`@derive(<UserTrait>)` is valid when the trait is non-generic and **every** method has a default body. The derive adopts the defaults wholesale, exactly like an empty `impl Trait for T {}`, and registers the trait membership, so the type satisfies `T: Trait` bounds and coerces to `dyn Trait`. A trait with a required, default-less method cannot be derived: E0050 names the missing methods, and you write the explicit `impl`. Because Noeta is reflection-first, a fully-defaulted trait can still do real per-type work, since its default bodies can reflect over `self` with `type_of` and `attributes_of`.

```noeta check
trait Describable {
    fn label(): string { return "thing" }
    fn describe(): string { return "a " ~ self.label() ~ "!" }
}
@derive(Describable)
struct Point { x: int }
echo Point { x: 1 }.describe()   // a thing!
```

## Derive errors

Deriving a built-in the compiler has no recipe for (`@derive(Add)`, `@derive(Validate)`) or with the wrong generic arity (`@derive(Comparable<int>)`, `@derive(Serialize)` without a format) is E0014. The refusal names the routes still open to *that* trait: `Add` delegates through a field, `Validate` wants an `impl`. Spelling a derive as the data-attribute `#[derive(...)]` is E0017, since `@derive` is a codegen directive rather than an attribute.

## Bridging a required member

A trait with required methods can still derive when you tell the machinery, or let it deduce, what to bridge them to (`@derive(Trait, member: target)`):

```noeta check
trait Ordered {
    fn value(): int
    fn less(other: Money): bool { return self.value() < other.value() }
}
@derive(Ordered, value: amount)
struct Money { amount: int }
```

The synthesized bridge is mechanical (`fn value(): int { return self.amount }`) and fully checked. With no explicit binding, deduction is deterministic: a field with the **same name** as the required method wins, else a **unique** type-compatible field; anything else is E0050 *listing the candidates*. A binding can also target an existing method, forwarded with the trait's arguments.

A bridge carries the trait method's **`async`-ness**: an `async` trait method synthesizes an `async` bridge, and a bridge onto an `async` method awaits it. The one refused combination is a *synchronous* trait method bound to an `async` method (E0050). The bridge would hand back the target's `Future<T>` under a declared `T`, and its body is not an async context, so there is nowhere legal to await it.

## Delegating through a field (`via:`)

`@derive(Trait, via: field)` forwards the whole trait through a field, giving the newtype pattern without boilerplate.

For a **user trait**, every method forwards into the field's own implementation, and the field's type must implement the trait. `async` methods included: the forwarder is `async` and awaits the field's future, so a delegated `async fn` is a `Future<T>` at the call site exactly like the one it forwards to.

For the **built-ins**, a template table covers `Equatable`/`Comparable`/`Display` (compare or render the fields), [`Error`](Error-Handling#deriving-error) (forward `message()` into the field's own), and the operator traits `Add`/`Sub`/`Mul`/`Div`/`Concat` (unwrap-op-rewrap, on single-field types only, since the result must construct a new value). Every other built-in is an `impl`:

```noeta check
@derive(Comparable, via: cents)
@derive(Add, via: cents)
struct Price { cents: int }
```

**What each template demands of the field is checked at the derive** (E0050), because a `via:` derive registers the trait's membership whichever field it names. A field that cannot answer the forwarded construct would give you a type that reports the trait, satisfies `is dyn Trait`, and then fails on the trait's one method.

The demand is the same question the forwarded construct itself asks: `Comparable` needs a field that **orders**, `Error` a field whose type implements `Error`, and each operator trait a field its operator accepts, so `@derive(Mul, via: label)` on a `string` field is refused while `@derive(Concat, via: label)` on the same field is fine. `Equatable` and `Display` demand nothing at all: `==` is universal, and the `Display` template **renders** the field rather than calling `to_string()` on it, so it delegates through any field type, reaching a type with a `to_string` of its own exactly as `${…}` does.

## Native derive recipes

An extension can register a derive (`ExtDerive`, see [Native Extensions](Native-Extensions)), and `@derive(<Name>)` then synthesizes methods forwarding into the extension's native handler. std ships `Inspect`, where `@derive(Inspect)` gives `inspect()`, a structural dump through the native JSON renderer.

A fully-defaulted user trait can do the same kind of structural work in pure Noeta, walking `self`'s fields with `fields_of(value)` (see [Attributes & Reflection](Attributes-and-Reflection)), and be derived onto any type.

## Field constraints (E0050)

A derive must be supportable by the type's fields, or by an enum's variant payloads. `Comparable` needs every field to have an ordering, and a `List`/`Map`/`Set`/tuple/`bytes`/function field can never order, so the derive is rejected at the declaration instead of failing at the first runtime comparison.

`Serialize` likewise rejects function-typed fields, unless the field is [`#[Transient]`](#keeping-a-field-out-of-the-wire), since a field outside the wire form needs no encoding. `Comparable` has no such escape, because ordering happens in-process where every field is present. Value-dependent kinds (`dyn`, unions, extern types like `Uuid`) stay permitted and defer to the runtime, and `Equatable` has no constraint at all, since structural `==` is total.

## Generic derives are conditional

`@derive(Comparable) struct Box<T> { value: T }` defers the parameter-typed field to each use: `Box<int>` satisfies `Comparable`, and `Box<List<int>>` fails the bound at the call site with E0025. A hand-written `impl` is the author's contract and stays unconditional.

`via:` composes with this. A parameter-typed via field defers to the instantiation site too, and the condition is the **via field's alone**, since delegation exists precisely so sibling fields do not constrain the trait. A `Slot<T>` with an `id: int` field and a `payload: T` field deriving `@derive(Comparable, via: id)` satisfies `Comparable` at every instantiation, `Slot<List<int>>` included, which a field-wise derive would refuse:

```noeta check
@derive(Comparable, via: value)
struct Box<T> {
    value: T
    note: string
}
fn smallest<T: Comparable>(x: T, y: T): T {
    return if x < y then x else y
}
echo smallest(Box { value: 1, note: "a" }, Box { value: 9, note: "b" }).value
```

## See also

- [Generics & Traits](Generics-and-Traits) — the built-in trait set, `impl Trait { }`, and coherence.
- [Attributes & Reflection](Attributes-and-Reflection) — `#[...]` data attributes, `type_of`, `fields_of`.
- [Error Handling](Error-Handling#deriving-error) — `@derive(Error)` in depth.
