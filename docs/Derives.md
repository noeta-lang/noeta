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
| `Serialize<Json>` | Synthesizes `to_json()` (on an enum: the variant rendering `json.stringify` produces). |

```noeta check
@derive(Equatable, Comparable, Display, Clone)
class Point {
    x: int
    y: int
    fn new(x: int, y: int): Point { return Point { x: x, y: y } }
}
echo Point.new(1, 2) < Point.new(1, 3)   // true

@derive(Serialize<Json>)
class User { name: string  id: int  active: bool }
echo User.new("Ada", 7, true).to_json()  // {"name":"Ada","id":7,"active":true}
```

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

Deriving a non-derivable trait (`@derive(Add)`) or wrong generic arity (`@derive(Comparable<int>)`, `@derive(Serialize)` without a format) is E0014. The old `#[derive(...)]` spelling is E0017.

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

## Delegating through a field (`via:`)

`@derive(Trait, via: field)` forwards the whole trait through a field — the newtype pattern without boilerplate. For a user trait, every method forwards into the field's own implementation (the field's type must implement the trait). For the built-ins, a template table covers `Equatable`/`Comparable`/`Display` (compare/render the fields) and the operator traits `Add`/`Sub`/`Mul`/`Div`/`Concat` (unwrap-op-rewrap; single-field types only, since the result must construct a new value):

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
