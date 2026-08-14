# Derives — @derive

`@derive(...)` generates trait impls from a type's shape. It is a *codegen* directive, distinct from `#[...]` data attributes (see [Attributes & Reflection](Attributes-and-Reflection)). Everything a derive synthesizes obeys the ordinary [coherence rule](Generics-and-Traits#coherence) — at most one implementation of a trait per type, however it got there.

## The derivable built-ins

| Derivable | Effect |
|---|---|
| `Equatable` | Structural equality. |
| `Comparable` | Field-wise ordering, in declaration order (recurses into nested objects and enum payloads). On an **enum**: variant declaration order first (`Low < Medium < High`), then payload fields. Also what `.sorted()` uses. |
| `Display` | A structural `to_string` — a **marker**: the structural default you already get, kept so a competing hand-written `impl Display` is a coherence error. |
| `Error` | `message()` returns `"${self}"` — the type's display story (a hand-written `impl Display`'s `to_string()`, or the structural rendering under `@derive(Display)`). Requires the type to have `Display` at all (E0050 otherwise); `@derive(Error, via: field)` instead forwards `message()` into the field's own `Error` implementation. See [Error Handling](Error-Handling#deriving-error). |
| `Clone` | A structural clone — a marker like `Display` (value semantics already copy). |
| `Serialize<Json>` | Synthesizes `to_json()` (on an enum: the variant rendering `json.stringify` produces). Encoding always writes **every** field — a default is a decode-side notion, so it never omits one. A `bytes` field encodes as a JSON **array of its byte values** (`[104, 105]`) — the lossless spelling that needs no agreed side-channel like base64; it is write-only, since `bytes` is not a decodable field type. An **enum value** encodes as its case name when the variant is payload-free (`"Green"`) and as `{"Case":[payload…]}` when it carries one (`Shape.Circle(3)` → `{"Circle":[3]}`) — the payload travels, positionally, rather than being dropped in favor of the tag; `Result` is a plain enum, so `Ok(5)` is `{"Ok":[5]}`. Like `bytes`, the payload-carrying form is write-only ([a payload-carrying enum has no JSON decoding at all](#enum-typed-fields)). An `Option` is the one exception, by the JSON-null convention: `some(x)` encodes as `x` and `none` as `null`, at the top level and inside a variant's payload alike. |
| `Deserialize<Json>` | Registers the type's decode recipe, so JSON decodes into it: `json.parse::<T>` / `json.try_parse::<T>`, `Response.json::<T>()`, and the router-facing `json.decode_typed("T", text)` (which resolves the type by *name* at runtime, and needs this derive). Derivable for a non-generic `struct` or `class` whose fields are all JSON-decodable — numbers, `bool`, `string`, `?T`, `List`, string-keyed `Map`, a declared **enum**, or another such type; anything else is E0050. A decoded `class` is a class: it has identity, compares by reference, and runs its `Validate` at the door like any other decode. See [which fields may be omitted](#which-json-fields-may-be-omitted) and [enum-typed fields](#enum-typed-fields) below. |

```noeta check
@derive(Equatable, Comparable, Display, Clone)
class Point {
    x: int
    y: int
    pub fn new(x: int, y: int): Point { return Point { x: x, y: y } }
}
echo Point.new(1, 2) < Point.new(1, 3)   // true

@derive(Serialize<Json>)
class User {
    name: string
    id: int
    active: bool
    pub fn new(name: string, id: int, active: bool): User { return User { name: name, id: id, active: active } }
}
echo User.new("Ada", 7, true).to_json()  // {"name":"Ada","id":7,"active":true}
```

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

The literal cutoff is not arbitrary: a decode is a pure data walk with no access to the running program, so it can only fill a default it carries *as data*. Every in-process constructor — a `T { … }` literal, `construct(name, fields)` — runs the field's compiled default expression instead, which is why those accept a non-literal default where a decode cannot. `field_specs_of::<T>()` reports `optional = true` for any default at all; a decode narrows that to the literal ones and tells you when it did.

The same rule governs request bodies: a web framework decodes a handler's body parameter with `json.decode_typed`, so a body struct with a literal default accepts documents that omit that field rather than rejecting them.

## A derive sees the whole type

`Serialize` writes every field and `Deserialize` reads every field, **private ones included** — and that is the point rather than an oversight.

A derive is written *inside* the declaration, next to the methods, so it is the type saying what its wire form is. That standing is what a caller-side reflective door does not have: `construct` refuses to set a private field by name, and `fields_of` reports only the fields you could have read yourself, because those are reached from outside by code the type never authorized. The two rules answer different questions — **`pub` governs access, a derive governs shape.**

The alternative makes a round trip lossy with nothing able to repair it. Encoding writes every field, so a wire form of the whole value exists whether or not anything can read it back; a decode that skipped private fields would build a value the encoder never described.

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

## Enum-typed fields

An enum-typed field decodes from the wire values the enum's own JSON Schema advertises — and only those, because describing a document with one vocabulary and decoding it with another is how a schema and a decoder come apart.

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

What you get back is a **real enum value**: it `match`es exhaustively and compares equal to a case written in source, not a string standing in for one.

A value that names no case is a path-carrying `JsonError` of kind `unknown_variant`, whose detail lists every accepted wire value — distinct from `mismatch`, which means the document has the wrong *shape* rather than an out-of-vocabulary value. Neither can panic, and neither can produce a silently-wrong value.

An enum with a **payload-carrying** variant has no JSON decoding at all, so a struct with such a field is E0050: a data-carrying sum has no canonical JSON spelling, and decoding only its payload-free half would accept documents against a schema that cannot describe the type. Build those cases with [`construct("Enum.Variant", payload)`](Attributes-and-Reflection#constructtfields-resultdyn-string--constructname-fields-resultdyn-string), or convert a single wire value with [`Enum.try_from`](Structs-Classes-and-Enums#enums).

Encoding is therefore deliberately **asymmetric** for such a variant: `json.stringify`/`to_json()` write it as `{"Case":[payload…]}` so nothing is lost on the way out, but no decode accepts that form on the way back. There is no round trip to break — the alternative was writing the bare tag and silently discarding the payload.

## Deriving a user trait

`@derive(<UserTrait>)` is valid when the trait is non-generic and **every** method has a default body — the derive adopts the defaults wholesale, exactly like an empty `impl Trait for T {}`, and registers the trait membership (so the type satisfies `T: Trait` bounds and coerces to `dyn Trait`). A trait with a required (default-less) method cannot be derived — E0050 names the missing methods; write the explicit `impl`. Because Noeta is reflection-first, a fully-defaulted trait can still do real per-type work: its default bodies can reflect over `self` (`type_of`, `attributes_of`) rather than needing a macro system.

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

Deriving a non-derivable trait (`@derive(Add)`) or wrong generic arity (`@derive(Comparable<int>)`, `@derive(Serialize)` without a format) is E0014. Spelling a derive as the data-attribute `#[derive(...)]` is E0017 — `@derive` is a codegen directive, not an attribute.

## Bridging a required member

A trait with required methods can still derive when you tell the machinery — or let it deduce — what to bridge them to (`@derive(Trait, member: target)`):

```noeta check
trait Ordered {
    fn value(): int
    fn less(other: Money): bool { return self.value() < other.value() }
}
@derive(Ordered, value: amount)
struct Money { amount: int }
```

The synthesized bridge is mechanical (`fn value(): int { return self.amount }`) and fully checked. With no explicit binding, deduction is deterministic: a field with the **same name** as the required method wins; else a **unique** type-compatible field; anything else is E0050 *listing the candidates*. A binding can also target an existing method (forwarded with the trait's arguments).

A bridge carries the trait method's **`async`-ness**: an `async` trait method synthesizes an `async` bridge, and a bridge onto an `async` method awaits it. The one refused combination is a *synchronous* trait method bound to an `async` method (E0050) — the bridge would hand back the target's `Future<T>` under a declared `T`, and its body is not an async context, so there is nowhere legal to await it.

## Delegating through a field (`via:`)

`@derive(Trait, via: field)` forwards the whole trait through a field — the newtype pattern without boilerplate. For a user trait, every method forwards into the field's own implementation (the field's type must implement the trait), `async` methods included — the forwarder is `async` and awaits the field's future, so a delegated `async fn` is a `Future<T>` at the call site exactly like the one it forwards to. For the built-ins, a template table covers `Equatable`/`Comparable`/`Display` (compare/render the fields) and the operator traits `Add`/`Sub`/`Mul`/`Div`/`Concat` (unwrap-op-rewrap; single-field types only, since the result must construct a new value):

```noeta check
@derive(Comparable, via: cents)
@derive(Add, via: cents)
struct Price { cents: int }
```

## Native derive recipes

An extension can register a derive (`ExtDerive` — see [Native Extensions](Native-Extensions)): `@derive(<Name>)` then synthesizes methods forwarding into the extension's native handler. std ships `Inspect` — `@derive(Inspect)` gives `inspect()`, a structural dump through the native JSON renderer. And with `fields_of(value)` (see [Attributes & Reflection](Attributes-and-Reflection)), a fully-defaulted user trait can do the same kind of structural work in pure Noeta — walk `self`'s fields reflectively — and be derived onto any type.

## Field constraints (E0050)

A derive must be supportable by the type's fields (or an enum's variant payloads): `Comparable` needs every field to have an ordering — a `List`/`Map`/`Set`/tuple/`bytes`/function field can never order, so the derive is rejected at the declaration instead of failing at the first runtime comparison. `Serialize` likewise rejects function-typed fields. Value-dependent kinds (`dyn`, unions, extern types like `Uuid`) stay permitted and defer to the runtime. `Equatable` has no constraint — structural `==` is total.

## Generic derives are conditional

`@derive(Comparable) struct Box<T> { value: T }` defers the parameter-typed field to each use: `Box<int>` satisfies `Comparable`, `Box<List<int>>` does not (the bound fails at the call site, E0025). A hand-written `impl` is the author's contract and stays unconditional.

`via:` composes with this: a parameter-typed via field defers to the instantiation site too, and the condition is the **via field's alone** — delegation exists precisely so sibling fields don't constrain the trait. A `Slot<T>` with an `id: int` field and a `payload: T` field deriving `@derive(Comparable, via: id)` satisfies `Comparable` at every instantiation (only `id` is compared), even `Slot<List<int>>`, which a field-wise derive would refuse:

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
