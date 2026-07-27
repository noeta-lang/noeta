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

Two boundaries hold statically. A generic method's own parameter does **not** forward into a call-site-typed position (`json.try_parse::<U>` inside a method body is E0058 — forwarding stays a top-level-generic-fn capability). And a **trait's** required-method set stays monomorphic — a per-method `<U>` on a trait method is E0058, since the trait is dispatched dynamically and each `impl` would have to agree on the parameter; put the generic method on a concrete type, or make the whole trait generic (`trait T<U> { ... }`).

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

Forwarding works from a **top-level generic function** and from a **nested `fn`** inside one, and a **composite** parameter forwards too (`List<T>`, `Box<T>` in a `::<...>` position), not just the bare `T`. The boundaries, all reported statically: a **generic method's** own parameter does not forward into a call-site-typed position (E0058); an instantiation the call site cannot pin must be spelled with a turbofish (E0023); and a function that forwards `T` this way is not usable as a bare value (E0058) — call it, or wrap it in a closure.

## Generic functions as values

A generic function referenced as a **value** in an expected-type position instantiates against the expectation and carries the precise monomorphic function type — no turbofish needed:

```noeta
fn wrap<T>(x: T): List<T> { return [x] }

ys = [1, 2, 3].map(wrap)             // List<List<int>> — wrap instantiates as (int) -> List<int>
f: (string) -> List<string> = wrap   // and here as (string) -> List<string>
```

Declared bounds ride along (`g: (Box, Box) -> Box = biggest` is E0025 when `Box` is not `Comparable`). A bare, expectation-free binding (`f = wrap`) keeps the honest gradual value: the erased signature, parameters `dyn`, calls deferred per position. The runtime value is the same erased function either way — only the static judgment changes.

## The built-in traits

Traits are a **fixed built-in set** — naming an unknown one is E0014. Operators dispatch to a trait's method:

| Trait | Method | Lights up |
|---|---|---|
| `Equatable` | `eq(other): bool` | `==` `!=` |
| `Comparable` | `compare(other): Ordering` | `< <= > >=` |
| `Display` | `to_string(): string` | `echo`, `${…}` |
| `Error` | `message(): string` | the idiomatic `Err` payload — see [Error Handling](Error-Handling) |
| `Validate` | `validate(): Result<void, E>` | a data-boundary invariant; auto-runs at typed decode — see [Validation](Validation) |
| `From<Source>` | `from(value: Source): Target` — associated | error conversion at `?` — see [Error Handling](Error-Handling#converting-errors-at---impl-fromsource) |
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

**Default methods fall back.** A user trait's method *with* a body is a **default**: an implementor may omit it, and the omitted method falls back to the trait's default body — hoisted onto the implementing type, so it dispatches like any method (a default that mentions `self` is an instance method and may call the trait's required methods; a self-less default is an associated fn, called on the type). A method the impl provides always overrides its default — and an override is held to the trait's signature exactly like a required method is. A **generic** trait implements at an instantiation — `impl Keyed<int> { … }`, `impl Keyed<string> for Tag { … }`, `@derive(Keyed<string>)` — and its defaults substitute the type parameters through their signatures and bodies before hoisting; a bare `impl Keyed`/`@derive(Keyed)` on a generic trait is an arity error naming the parameters.

**A method the impl *provides* is an instance method**, whatever its body happens to mention. A trait method that does not need `self` (`fn greet(who: string): string { return "hi " ~ who }`) is still part of the trait's instance interface and is called on a value; the in-body and standalone spellings agree on this. (Only an *omitted* default is classified from its body, per the rule above.)

**The signature is the contract, `async` included.** An implementation must match the trait's declaration in arity, parameter types, return type, *and* `async`-ness; a mismatch is E0015. `async` belongs on that list because every receiver form types a call from *some* signature and they must all agree: an `async fn m(): T` is called for a `Future<T>` and a plain `fn m(): T` for a `T`, so a synchronous method satisfying an `async` declaration (or the reverse) would make a bound's and a trait object's typing a promise the implementation does not keep.

## Trait objects

A `dyn Trait` parameter or binding accepts any implementor and dispatches on the value's concrete type at run time, while typing statically from the trait's declaration — return type, parameter types, arity, and whether the method is `async`:

```noeta
use std.task.{sleep}

trait Fetcher {
    async fn fetch(url: string): string
}

struct Http {
    impl Fetcher {
        async fn fetch(url: string): string {
            sleep(1).await
            return "body:" ~ url
        }
    }
}

async fn via_dyn(f: dyn Fetcher): string {
    return f.fetch("one").await     // Future<string>, exactly as through a `<F: Fetcher>` bound
}
echo via_dyn(Http {}).await         // body:one
```

The bound and the trait object agree by construction: both read the trait's declaration, so a call is typed identically whether the receiver is a bound parameter, a declared `dyn Trait`, a `dyn` narrowed with `x is dyn Trait`, or the concrete type. A wrong argument type or arity through `dyn` is E0007, just as it is through a bound.

`dyn` on a **generic** trait erases its parameters — there is no `dyn Trait<...>` surface form, so, as with a bare `<T: Store>` bound, the parameters instantiate permissively to `dyn` and those positions defer to run time. Name the instantiation with a bound (`<S: Store<int>>`) when you need them typed.

## `@derive` — synthesized implementations

`@derive(...)` generates trait impls from a type's shape — a *codegen* directive, distinct from `#[...]` data attributes (see [Attributes & Reflection](Attributes-and-Reflection)):

```noeta check
@derive(Equatable, Comparable, Display, Clone)
class Point {
    x: int
    y: int
    fn new(x: int, y: int): Point { return Point { x: x, y: y } }
}
echo Point.new(1, 2) < Point.new(1, 3)   // true
```

The derivable built-ins are `Equatable`, `Comparable`, `Display`, `Error`, `Clone`, and `Serialize<Json>`; a fully-defaulted user trait derives too, and the `member:`/`via:` bindings bridge or delegate what a plain derive cannot reach. The full story — the derivable table, user-trait derives, bridging, delegation, native recipes, field constraints, and conditional generic derives — lives on [Derives](Derives).

## Coherence

Each type has **at most one** implementation of a given trait, across `@derive(T)`, an in-body `impl T { }`, and a standalone `impl T for Type { }`. A duplicate or competing impl is E0027. Because all traits are built-in and `impl` blocks may only target a type declared in the same module, the orphan problem is structurally impossible — coherence checking reduces to uniqueness.

## Operator errors

Applying an operator to a type that does not implement its trait is an error: `+` or `<` on a plain struct is E0007; `===` on a value type (a struct) is E0034 (identity is a class-only concept).
