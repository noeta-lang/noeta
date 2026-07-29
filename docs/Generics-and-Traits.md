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

A method's own parameter **forwards** into a call-site-typed position exactly as a free function's does — `json.try_parse::<U>` inside a method body is fine (see [Forwarding `T`](#forwarding-t) for the one thing to know about it). One boundary still holds statically: a **trait's** required-method set stays monomorphic — a per-method `<U>` on a trait method is E0058, since the trait is dispatched dynamically and each `impl` would have to agree on the parameter; put the generic method on a concrete type, or make the whole trait generic (`trait T<U> { ... }`).

## Forwarding `T`

Inside a generic function's body, `T` is legal wherever a type goes — including as a turbofish argument to another generic, to a call-site-typed native function (`json.try_parse::<T>`), to `attributes_of::<T>()`, to `type_name::<T>()`, and to `channel::<T>(cap)`:

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

Forwarding works from a **top-level generic function**, from a **nested `fn`** inside one, and from a **generic method** — its own `<T>`, not the class's. A **composite** parameter forwards too (`List<T>`, `Box<T>` in a `::<...>` position), not just the bare `T`.

The boundaries. An instantiation the call site cannot pin must be spelled with a turbofish (E0023), and a function that forwards `T` this way is not usable as a bare value (E0058) — call it, or wrap it in a closure. A generic **type**'s parameter does not forward from a method body (E0058): it reaches the body through the receiver's type tag, which records the instantiation's *name* but no build recipe, so take the type as the method's own parameter instead.

One more, and it is about **spelling** rather than about what a parameter is. The slots a body forwards through are computed by a pass that runs before checking and reads the source as written, so it registers a forward from a call spelled on a **bare name** — `json.try_parse::<T>(s)`, `f::<T>(x)`, `self.load::<T>(s)`, `store.load::<T>(s)`, `Store.load::<T>(s)`. A receiver that is itself an expression (`self.inner.load::<T>(s)`) names its callee only once checking has typed it, which is too late, and is E0058. Bind the receiver first and the call is spelled on a name again:

```noeta check
use std.json
use std.json.JsonError

struct Order { id: int }

class Store {
    pub tag: string
    fn new(): Store { return Store { tag: "s" } }

    fn load<T>(text: string): Result<T, JsonError> {
        echo self.tag
        return json.try_parse::<T>(text)
    }
}

class Cache {
    pub inner: Store
    fn new(): Cache { return Cache { inner: Store.new() } }

    fn get<T>(text: string): Result<T, JsonError> {
        s = self.inner                  // `self.inner.load::<T>(text)` here would be E0058
        return s.load::<T>(text)
    }
}

c = Cache.new()
o = c.get::<Order>("{\"id\": 1}")
```

The last one is a **runtime** boundary, and the only one that is. A forwarding method must be called by a name the checker can resolve a receiver type for, because that is what pins the instantiation. The four dynamic ways into a method — a `dyn` receiver, a bound handle (`v.m`), an unbound handle (`T.m`), and `invoke(v, "m", args)` — carry none, so a forwarding method reached through one of them **aborts** naming the callee rather than guessing. It is the same judgment `type_name::<T>()` makes on a value built at no known instantiation: a plausible-looking wrong type would travel silently, and the fix belongs at the call site that lost it.

```noeta error
use std.json
use std.json.JsonError

struct Order {
    id: int
}

class Loader {
    pub label: string

    fn new(label: string): Loader {
        return Loader { label: label }
    }

    fn load<T>(text: string): Result<T, JsonError> {
        echo self.label
        return json.try_parse::<T>(text)
    }
}

l = Loader.new("orders")
o = l.load::<Order>("{\"id\": 1}")     // fine — the receiver's type resolves `Loader.load`

d: dyn = l
d.load("{\"id\": 1}")                 // aborts: no instantiation reaches here
```

### Asking what `T` is called

`type_name::<T>()` forwards too, and it is the cheapest forward there is: it wants the instantiation's **name** and nothing else, so it rides the same hidden slot with no recipe involved — which means it also serves an instantiation that *has* no recipe, a `class` for instance.

```noeta check
use std.{json}
use std.json.JsonError

struct Order { id: int }

fn decode<T>(text: string): Result<T, JsonError> {
    echo "decoding ${type_name::<T>()}"
    return json.try_parse::<T>(text)
}

echo decode::<Order>("{\"id\": 1}")
```

The answer is the **qualified** identity, byte-identical to what the concrete `type_name::<Order>()` yields at the same site — `namespace`, `use … as` alias and rename all followed. That agreement is the whole contract: the string exists to key a name-keyed registry, so a forwarded parameter answering the short name would silently miss every namespaced type.

It is also what opens the *other* name-keyed queries to a generic body. Their **turbofish** arm stays a compile-time key, so `field_specs_of::<T>()`, `variants_of::<T>()` and `construct::<T>(…)` over a parameter remain E0058 — but each has a **runtime-string** arm, and `type_name::<T>()` now supplies it:

```noeta check
struct Order { id: int }

fn field_count<T>(): int {
    return field_specs_of(type_name::<T>()).len()   // the real schema, per instantiation
}

echo field_count::<Order>()
```

The E0058 diagnostic points at exactly this route wherever it is open. See [Reflection over a type parameter](Attributes-and-Reflection#reflection-over-a-type-parameter).

The two channels are separate and both reach a method body, so a method's own `<U>` and its class's `<K>` can be asked about in the same expression: `U` resolves through the slot the call supplied, `K` off the receiver's type tag. Where a name shadows, the method's own wins — ordinary scoping.

What stays E0058 is a site **no** channel reaches: a self-less member of a generic type, whose parameter rides a receiver it does not have. A *composite* turbofish is unaffected either way: `type_name::<List<T>>()` heads at `List` whatever `T` is, so it stays the compile-time constant it always was.

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

There is also a **standalone** `impl Trait for T { ... }`, which must target a struct, class, or enum **the program declares** — a target no module declares is E0013, a wrong or missing required method is E0015. "The program" is the whole linked program rather than the one file: a module may implement a trait for a sibling module's type, or for a type declared in the entry, so where in your package you put an impl is a matter of layout. Across a *package* boundary it is not free: the [orphan rule](#the-orphan-rule) requires the impl to sit with the trait or with the type (E0070). A standalone impl of a **user** trait may carry method bodies (hoisted onto the target type); an impl of a **built-in** trait must stay an empty-body marker (a body is E0015 — those methods live in the type's own body).

**A standalone impl travels with its type.** Wherever the target type reaches — a sibling module, a consumer of your package, a consumer that never names the type and only ever holds it as a `dyn Trait` — its standalone impls go with it, and every surface that reads trait membership sees them: the `dyn Trait` coercion, trait-method dispatch, the precise `x is dyn Trait` test, a `<T: Trait>` bound, and `traits_of(x)`. Which spelling you choose is a matter of layout, never of reach — an in-body `impl Trait { … }` block and a standalone `impl Trait for T { … }` declare the same thing, and a type that must be written one way to be usable from another module is a bug, not a rule.

**Default methods fall back.** A user trait's method *with* a body is a **default**: an implementor may omit it, and the omitted method falls back to the trait's default body — hoisted onto the implementing type, so it dispatches like any method (a default that mentions `self` is an instance method and may call the trait's required methods; a self-less default needs no receiver and may be called either way — see the receiver rule below). A method the impl provides always overrides its default — and an override is held to the trait's signature exactly like a required method is. A **generic** trait implements at an instantiation — `impl Keyed<int> { … }`, `impl Keyed<string> for Tag { … }`, `@derive(Keyed<string>)` — and its defaults substitute the type parameters through their signatures and bodies before hoisting; a bare `impl Keyed`/`@derive(Keyed)` on a generic trait is an arity error naming the parameters.

**Which receiver a method takes is derived, not declared.** A method whose body mentions `self` is an **instance** method: call it on a value, `x.m(…)`. A self-less one that belongs to no trait is an **associated** function: call it on the type, `T.m(…)`. Getting that backwards is E0047 either way — an associated call has no receiver to bind `self` to, and an instance call on an associated function would evaluate a receiver and discard it.

A self-less method that a **trait** supplies is the third case, and it accepts **both**. The trait's contract puts it in the instance interface — that is how `dyn Trait` reaches it — while its body needs no receiver, so `T.m(…)` is equally well-defined; both spellings run the same code. This holds identically for an in-body `impl Trait { … }` block, a standalone `impl Trait for T { … }`, and a hoisted default, so how you spell the implementation never changes how you may call it:

```noeta
trait Greeter { fn greet(who: string): string }

struct En {
    id: int
    impl Greeter {
        fn greet(who: string): string { return "hi " ~ who }
    }
}
echo En.greet("Ada")             // hi Ada — on the type
echo En { id: 1 }.greet("Bob")   // hi Bob — on a value
```

(`From` is the exception: its `from` is a conversion that *builds* a value rather than acting on one, so it stays associated-only — and a `from` body that mentions `self` is E0015.)

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

Coherence has two halves: **uniqueness** — at most one implementation per (type, trait) pair — and the **orphan rule** — who is allowed to write one.

### Uniqueness

Each type has **at most one** implementation of a given trait, across `@derive(T)`, an in-body `impl T { }`, and a standalone `impl T for Type { }`. A duplicate or competing impl is E0027 — including two *different modules* that each implement one trait for one type, which is a conflict like any other and is reported rather than silently resolved in someone's favor. The diagnostic labels **both** sites, each rendered against its own file, since the two are routinely in different modules.

Uniqueness is always decidable here, and that is a property of the language: Noeta links **one whole program** at a time, so every module and every dependency is resolved into a single program before it is checked, and there is no separately-compiled unit that could hold an implementation this one cannot see.

### The orphan rule

An `impl Trait for Type` must live in the **same package** as the trait **or** as the type. A package that declares neither is E0070.

This is Rust's orphan rule with *crate* read as *package*, and — unlike Rust's — it is not there to make coherence decidable. It is there because the alternative is invisible action at a distance. Without it, a package deep in your dependency graph can implement one vendor's trait for another vendor's type, and the behavior appears in an application that imports both and names the implementing package nowhere:

```noeta ignore
// third.glue — depends on vendor.a and vendor.b, and is written by neither
impl Speaks for Thing { fn speak(): string { return "glue says ${self.id}" } }
```
```noeta ignore
// your application, which imports vendor.a and vendor.b and never mentions third.glue
t = Thing.new(7)
echo t is dyn Speaks   // true — from a package you did not write down
echo t.speak()         // "glue says 7"
```

Two such packages anywhere in one graph then collide as an E0027 the end user **cannot fix**: they own neither implementation, so they can remove neither, and the only escape is to drop a dependency. The cost of the feature is global and invisible; its benefit — attaching behavior to a foreign type — is already served by the newtype, below.

What the rule does **not** restrict:

- **Cross-module impls inside one package.** The boundary is the package, not the file. A module may implement a trait for a type declared in a sibling module, or in the entry, exactly as before.
- **`@derive(Trait)` and in-body `impl Trait { }`.** Both sit on the type's own declaration, so they are same-package by construction.
- **Cross-*package* impls with one end at home.** Implementing your own trait for a foreign type is fine; so is implementing a foreign trait for your own type. Only the third-party case is refused.

A **built-in** trait (`Display`, `Comparable`, …) and a trait provided by a native extension belong to no package, so they can never be the local end: `impl Display for SomeoneElsesType {}` has to live in that type's package.

### The escape hatch: a newtype

To give a foreign type behavior from your own package, wrap it in a type you own. `@derive(Trait, via: field)` is [the newtype pattern without the boilerplate](Derives#delegating-through-a-field-via) — it forwards the whole trait through the field:

```noeta ignore
@derive(Speaks, via: inner)
class MyThing { pub inner: Thing }
```

The behavior is then yours, scoped to your type, and visible to exactly the code that asks for it. E0070's help prints this sketch with your own names filled in.

## Operator errors

Applying an operator to a type that does not implement its trait is an error: `+` or `<` on a plain struct is E0007; `===` on a value type (a struct) is E0034 (identity is a class-only concept).
