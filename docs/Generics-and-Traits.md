# Generics & Traits

Generic types and functions, the built-in trait set that operators dispatch through, and `@derive`.

## Generics

Type parameters `<T>`, `<A, B>` go on functions, structs, classes, and enums. They are **erased at runtime** (one shape serves every instantiation).

```noeta
class Box<T> {
    pub value: T
    fn new(v: T): Box<T> { return Box { value: v } }
    fn get(): T { return self.value }
}

struct Pair<A, B> { first: A  second: B }

enum Opt<T> { None; Some(value: T) }
```

The element type is **tracked through instances and literals**: `Box.new(42)` is a `Box<int>`, and `Box { value: "hi" }` is a `Box<string>`. A mismatch downstream is E0007.

## Explicit instantiation — the turbofish

Any generic function can be instantiated explicitly with `f::<T, ...>(args)`. The type arguments bind to the declared type parameters **in order** (one per parameter — an arity mismatch is E0058, a turbofish on a non-generic function too), and they **win** over argument inference: an argument that disagrees with the explicit binding fails assignability against the substituted parameter (E0007) rather than silently re-inferring `T`.

```noeta
fn empty<T>(): List<T> { return [] }
fn pick<T>(a: T, b: T, first: bool): T { if first { return a } return b }

xs: List<int> = empty::<int>()      // T appears only in the return — turbofish carries it
echo pick::<string>("x", "y", false)
```

The expected type at an annotated binding can carry the instantiation too — `r: List<int> = empty()` infers `T = int` from the **return position** (the bidirectional checker seeds `T` from the expectation, and the arguments fill only what it leaves open). Inference flows through `?` and `??` as well: `o: Order = load(text)?` seeds `T = Order` by inverting the `Result` wrapper at the checked `Try`, and `o: Order = load(text) ?? fallback` through the coalesce — no turbofish needed.

## Generic methods

A method may declare **its own** type parameters, instantiated per call — independently of the class's parameters, which the receiver pins. `Box<T>` fixes `T` from the value; a method `pick<U>(...)` adds `U` on top, and both substitutions compose (`Box<int>` with `pick::<string>(...)` binds `T = int`, `U = string`).

```noeta
class Box<T> {
    pub value: T
    fn new(v: T): Box<T> { return Box { value: v } }
    fn paired<U>(u: U): Pair<T, U> { return Pair { first: self.value, second: u } }
}
struct Pair<A, B> { first: A  second: B }

b = Box.new(7)
p = b.paired::<string>("hi")        // Pair<int, string> — T from b, U from the turbofish
```

All three instantiation paths a free function has apply, consistent with them: **argument inference** (`b.choose(99, true)` infers `U = int`), the **member turbofish** `recv.m::<U>(args)` (the class's parameter never appears among these — only the method's own, arity-checked E0058), and **expected-type seeding** (`xs: List<int> = b.collect()` seeds a return-only `U` from the annotation). Bounds on a method parameter ride the ordinary trait machinery (`fn bigger<U: Comparable>(...)`; a non-`Comparable` argument is E0025). The parameters are erased like every generic — one compiled method serves every instantiation.

Two boundaries hold statically. A generic method's own parameter does **not** forward into a call-site-typed position (`json.try_parse::<U>` inside a method body is E0058 — method dispatch has no hidden-slot channel; forwarding stays a top-level-generic-fn capability). And a **trait's** required-method set stays monomorphic — a per-method `<U>` on a trait method is E0058, since the trait is dispatched dynamically and each `impl` would have to agree on the parameter; put the generic method on a concrete type, or make the whole trait generic (`trait T<U> { ... }`).

## Forwarding `T`

Inside a generic function's body, `T` is legal wherever a type goes — including as a turbofish argument to another generic, to a call-site-typed native function (`json.try_parse::<T>`), to `attributes_of::<T>()`, and to `channel::<T>(cap)`:

```noeta check
use std.{json}
use std.json.JsonError

struct Order { id: int }
struct User { name: string }

fn load<T>(text: string): Result<T, JsonError> {
    return json.try_parse::<T>(text)
}

order = load::<Order>("{\"id\": 1}")
user  = load::<User>("{\"name\": \"Ada\"}")   // same body, per-instantiation decode
```

Generics are erased, so one compiled body serves every instantiation; where a forwarded site needs per-instantiation data at runtime (a decode recipe, an attribute type's name), the instantiating call passes it through a hidden argument — invisible in the surface language. Forwarding works from a **top-level generic function** and from a **nested `fn`** inside one, and a **composite** parameter forwards too (`List<T>`, `Box<T>` in a `::<...>` position), not just the bare `T`. The boundaries, all reported statically: a **generic method's** own parameter does not forward into a call-site-typed position (E0058 — method dispatch has no hidden-slot channel); an instantiation the call site cannot pin must be spelled with a turbofish (E0023); and a function that forwards `T` this way is not usable as a bare value (E0058) — call it, or wrap it in a closure.

## Generic functions as values

A generic function referenced as a **value** in an expected-type position instantiates against the expectation and carries the precise monomorphic function type — no turbofish needed:

```noeta
fn wrap<T>(x: T): List<T> { return [x] }

ys = [1, 2, 3].map(wrap)             // List<List<int>> — wrap instantiates as (int) -> List<int>
f: (string) -> List<string> = wrap   // and here as (string) -> List<string>
```

Declared bounds ride along (`g: (Box, Box) -> Box = biggest` is E0025 when `Box` is not `Comparable`). A bare, expectation-free binding (`f = wrap`) keeps the honest gradual value: the erased signature, parameters `dyn`, calls deferred per position. The runtime value is the same erased function either way — only the static judgment changes.

## Bounds

Constrain a parameter to a built-in trait with `<T: Trait>`:

```noeta
fn max<T: Comparable>(a: T, b: T): T {
    if a > b { return a }
    return b
}
echo max(3, 9)          // 9
echo max("a", "b")      // b
```

Bounds are enforced statically at both ends:

- **Body-side requirement** — using `>` requires `Comparable`, `+` requires `Add`; on an *unbounded* `T` that operation is E0025 at the definition.
- **Call-site check** — instantiating with a type that does not satisfy the bound is E0025. The first argument pins `T`; later arguments are checked against that substitution (E0007 on mismatch).
- An unknown bound name is E0014.

**Instantiated bounds.** A bound on a *generic user trait* may demand a specific instantiation: `<T: Keyed<int>>` is satisfied only by a type with an `impl Keyed<int>` (an `impl Keyed<string>` fails the bound, E0025 naming `Keyed<int>`). A bare `<T: Keyed>` accepts any instantiation. A bound argument may name a sibling parameter — `<K, T: Keyed<K>>` ties the two together, with `K` pinned by the call's arguments:

```noeta check
trait Keyed<K> {
    fn key(): K
    fn same(other: K): bool {
        return self.key() == other
    }
}
struct Door {
    code: int
    impl Keyed<int> {
        fn key(): int {
            return self.code
        }
    }
}
fn matches<K, T: Keyed<K>>(item: T, k: K): bool {
    return item.same(k)
}
echo matches(Door { code: 7 }, 7)     // true
```

An instantiated bound must match the trait's arity (`T: Keyed<int, string>` on a one-parameter trait is E0014), and built-in traits take no bound arguments. (`From<Source>` is the one built-in whose *impl* carries a type argument — `impl From<JsonError> { … }` — but it is not usable as an instantiated bound.)

The bound also types the **body**: on a `T`-typed value, a method the bound's trait declares resolves at the bound's instantiation — under `<T: Keyed<int>>`, `item.key()` is an `int` and `item.same(x)` demands an `int`, so a wrong argument, return, or arity is E0007 at the definition, before any call site exists. A method no bound declares stays leniently deferred.

## The built-in traits

Traits are a **fixed built-in set** — naming an unknown one is E0014. Operators dispatch to a trait's method:

| Trait | Method | Lights up |
|---|---|---|
| `Equatable` | `eq(other): bool` | `==` `!=` |
| `Comparable` | `compare(other): Ordering` | `< <= > >=` |
| `Display` | `to_string(): string` | `echo`, `${…}` |
| `Error` | `message(): string` | the idiomatic `Err` payload — see [Error Handling](Error-Handling) |
| `From<Source>` | `from(value: Source): Target` — associated | error conversion at `?` — see [Error Handling](Error-Handling#converting-errors-at--impl-fromsource) |
| `Add` | `add(other): T` | `+` |
| `Sub` | `sub(other): T` | `-` |
| `Mul` | `mul(other): T` | `*` |
| `Div` | `div(other): T` | `/` |
| `Concat` | `concat(other): T` | `~` |
| `TryAdd` | `try_add(other): Result<T, E>` | fallible `+` (via `?`) |
| `Index` | `get(i): T` | `a[i]` |
| `Length` | `len(): int` | `x.len()` on a `<T: Length>` parameter |
| `Iterable` | `iter(): Iterator<T>` | `for x in o` |
| `Callable` | `call(...)` — any arity | `obj(args)` |
| `Clone` | — | structural clone |

`Ordering` is a namable built-in enum (`Ordering.Less` / `Equal` / `Greater`); calling `.compare()` on a primitive returns it.

## Implementing a trait

Implement a trait **in the type's body** with `impl Trait { }` (uniform across class, struct, and enum):

```noeta
class Money {
    amount: int
    fn new(a: int): Money { return Money { amount: a } }
    impl Add {
        fn add(other: Money): Money { return Money { amount: self.amount + other.amount } }
    }
    impl Comparable {
        fn compare(other: Money): Ordering { return self.amount.compare(other.amount) }
    }
}
echo (Money.new(3) < Money.new(5))   // true
```

**`Callable` makes an object invocable.** A type implementing `Callable` with a `call` method can be applied like a function — `obj(args)` dispatches to `obj.call(args)`, with the receiver's state in scope. The arity is the method's own (the protocol does not pin it), and the call is arity/argument-checked against the method's signature like any method call. A user type without a `call` method is statically not callable (E0007).

```noeta
class Adder {
    pub base: int
    impl Callable {
        fn call(x: int): int { return self.base + x }
    }
}
add10 = Adder { base: 10 }
echo add10(5)     // 15
```

There is also a **standalone** `impl Trait for T { ... }`, which must target a type declared in the same module — an orphan target is E0013, a wrong or missing required method is E0015. A standalone impl of a **user** trait may carry method bodies (hoisted onto the target type); an impl of a **built-in** trait must stay an empty-body marker (a body is E0015 — those methods live in the type's own body).

**Default methods fall back.** A user trait's method *with* a body is a **default**: an implementor may omit it, and the omitted method falls back to the trait's default body — hoisted onto the implementing type, so it dispatches like any method (a default that mentions `self` is an instance method and may call the trait's required methods; a self-less default is an associated fn, called on the type). A method the impl provides always overrides its default. A **generic** trait implements at an instantiation — `impl Keyed<int> { … }`, `impl Keyed<string> for Tag { … }`, `@derive(Keyed<string>)` — and its defaults substitute the type parameters through their signatures and bodies before hoisting; a bare `impl Keyed`/`@derive(Keyed)` on a generic trait is an arity error naming the parameters.

## `@derive` — synthesized implementations

`@derive(...)` generates trait impls from a type's shape. It is a *codegen* directive, distinct from `#[...]` data attributes (see [Attributes & Reflection](Attributes-and-Reflection)).

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

**Deriving a user trait.** `@derive(<UserTrait>)` is valid when the trait is non-generic and **every** method has a default body — the derive adopts the defaults wholesale, exactly like an empty `impl Trait for T {}`, and registers the trait membership (so the type satisfies `T: Trait` bounds and coerces to `dyn Trait`). A trait with a required (default-less) method cannot be derived — E0050 names the missing methods; write the explicit `impl`. Because Noeta is reflection-first, a fully-defaulted trait can still do real per-type work: its default bodies can reflect over `self` (`type_of`, `attributes_of`) rather than needing a macro system.

```noeta check
trait Describable {
    fn label(): string { return "thing" }
    fn describe(): string { return "a " ~ self.label() ~ "!" }
}
@derive(Describable)
struct Point { x: int }
echo Point { x: 1 }.describe()   // a thing!
```

Errors: deriving a non-derivable trait (`@derive(Add)`) or wrong generic arity (`@derive(Comparable<int>)`, `@derive(Serialize)` without a format) is E0014. The old `#[derive(...)]` spelling is E0017.

**Bridging a required member.** A trait with required methods can still derive when you tell the machinery — or let it deduce — what to bridge them to (`@derive(Trait, member: target)`):

```noeta check
trait Ordered {
    fn value(): int
    fn less(other: Money): bool { return self.value() < other.value() }
}
@derive(Ordered, value: amount)
struct Money { amount: int }
```

The synthesized bridge is mechanical (`fn value(): int { return self.amount }`) and fully checked. With no explicit binding, deduction is deterministic: a field with the **same name** as the required method wins; else a **unique** type-compatible field; anything else is E0050 *listing the candidates*. A binding can also target an existing method (forwarded with the trait's arguments).

**Delegating through a field (`via:`).** `@derive(Trait, via: field)` forwards the whole trait through a field — the newtype pattern without boilerplate. For a user trait, every method forwards into the field's own implementation (the field's type must implement the trait). For the built-ins, a template table covers `Equatable`/`Comparable`/`Display` (compare/render the fields) and the operator traits `Add`/`Sub`/`Mul`/`Div`/`Concat` (unwrap-op-rewrap; single-field types only, since the result must construct a new value):

```noeta check
@derive(Comparable, via: cents)
@derive(Add, via: cents)
struct Price { cents: int }
```

**Native derive recipes.** An extension can register a derive (`ExtDerive` — see [Native Extensions](Native-Extensions)): `@derive(<Name>)` then synthesizes methods forwarding into the extension's native handler. std ships `Inspect` — `@derive(Inspect)` gives `inspect()`, a structural dump through the native JSON renderer. And with `fields_of(value)` (see [Attributes & Reflection](Attributes-and-Reflection)), a fully-defaulted user trait can do the same kind of structural work in pure Noeta — walk `self`'s fields reflectively — and be derived onto any type.

**Field constraints (E0050).** A derive must be supportable by the type's fields (or an enum's variant payloads): `Comparable` needs every field to have an ordering — a `List`/`Map`/`Set`/tuple/`bytes`/function field can never order, so the derive is rejected at the declaration instead of failing at the first runtime comparison. `Serialize` likewise rejects function-typed fields. Value-dependent kinds (`dyn`, unions, extern types like `Uuid`) stay permitted and defer to the runtime. `Equatable` has no constraint — structural `==` is total.

**Generic derives are conditional.** `@derive(Comparable) struct Box<T> { value: T }` defers the parameter-typed field to each use: `Box<int>` satisfies `Comparable`, `Box<List<int>>` does not (the bound fails at the call site, E0025). A hand-written `impl` is the author's contract and stays unconditional.

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

## Coherence

Each type has **at most one** implementation of a given trait, across `@derive(T)`, an in-body `impl T { }`, and a standalone `impl T for Type { }`. A duplicate or competing impl is E0027. Because all traits are built-in and `impl` blocks may only target a type declared in the same module, the orphan problem is structurally impossible — coherence checking reduces to uniqueness.

## Operator errors

Applying an operator to a type that does not implement its trait is an error: `+` or `<` on a plain struct is E0007; `===` on a value type (a struct) is E0034 (identity is a class-only concept).
