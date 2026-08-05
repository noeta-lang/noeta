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

### Instantiating a generic *type* at the call site

The turbofish above instantiates a **function**. A generic **type** takes one too, on the receiver of a static call:

```noeta
struct Todo { id: int }

class Repo<T> {
    pub tbl: string
    pub fn new(tbl: string): Repo<T> { return Repo { tbl: tbl } }
    pub fn model(): string { return self.tbl ~ ":" ~ type_name::<T>() }
}

r = Repo::<Todo>.new("todos")       // Repo<Todo> — the call says so itself
echo r.model()                      // todos:Todo
```

`Type::<Args>.method(args)` works for **any** static function, not just one called `new` — Noeta has no constructor concept, so `Repo::<Todo>.open(dsn)` and `Repo::<Todo>.from_dsn(dsn)` read the same way. The type arguments bind to the type's declared parameters in order, arity-checked (E0058, as is a turbofish on a non-generic type). They are the *class's* parameters, and only those: a method's own `<U>` is instantiated on the method, so `Repo.new::<Todo>` is an error (`new` declares no type parameters of its own). Where both are generic, both are spelled — see [Both at once](#both-at-once).

The `::` is required: `Repo<Todo>.new(…)` is a parse error, because a bare `<` after an identifier is ambiguous with less-than in expression position. That is the same reason every other type argument in the language is written with a turbofish.

**When to reach for it.** Prefer an annotation or a declared type where the position already has one — it documents the value where the reader looks for it, and inference is the language's declared direction. Four positions supply the instantiation that way, and all of them work:

```noeta
struct Todo { id: int }
struct User { id: int }
struct Note { id: int }
struct Tag { id: int }

class Repo<T> {
    pub tbl: string
    pub fn new(tbl: string): Repo<T> { return Repo { tbl: tbl } }
}
class Holder {
    inner: Repo<Note>
    pub fn new(i: Repo<Note>): Holder { return Holder { inner: i } }
}
fn users(): Repo<User> { return Repo.new("users") }         // a declared return
fn describe(r: Repo<Tag>): string { return r.tbl }           // a parameter's type

rt: Repo<Todo> = Repo.new("todos")                           // an annotated binding
h = Holder.new(Repo.new("notes"))                            // a field's declared type
echo describe(Repo.new("tags"))
```

Reach for the call-site turbofish when **none** of them does — the value is echoed straight out, passed to a `dyn` parameter, or simply bound with no annotation you want to write. A construction that records no instantiation at all is a check-time E0058 whose help names this spelling, rather than a clean check and an abort at the first attempt to read `T` back off the value.

If an annotation and a call-site turbofish **disagree**, the turbofish wins the instantiation and the disagreement is an ordinary assignability error at the binding:

```noeta error
struct Todo { id: int }
struct User { id: int }

class Repo<T> {
    pub tbl: string
    pub fn new(tbl: string): Repo<T> { return Repo { tbl: tbl } }
}

r: Repo<User> = Repo::<Todo>.new("todos")   // E0007 — a Repo<Todo> is not a Repo<User>
```

That is not a special rule. `Repo::<Todo>.new("todos")` is a self-sufficient expression of type `Repo<Todo>`, exactly as `5` is an `int`, so the mismatch is reported by the same rule and at the same span as `n: int = "s"`.

## Generic methods

A method may declare **its own** type parameters, instantiated per call — independently of the class's parameters, which the receiver pins. `Box<T>` fixes `T` from the value; a method `pick<U>(...)` adds `U` on top, and both substitutions compose (`Box<int>` with `pick::<string>(...)` binds `T = int`, `U = string`).

```noeta
class Box<T> {
    pub value: T
    pub fn new(v: T): Box<T> { return Box { value: v } }
    pub fn paired<U>(u: U): Pair<T, U> { return Pair { first: self.value, second: u } }
}
struct Pair<A, B> { first: A  second: B }

b = Box.new(7)
p = b.paired::<string>("hi")        // Pair<int, string> — T from b, U from the turbofish
```

All three instantiation paths a free function has apply, consistent with them: **argument inference** (`b.choose(99, true)` infers `U = int`), the **member turbofish** `recv.m::<U>(args)` (the class's parameter never appears among these — only the method's own, arity-checked E0058), and **expected-type seeding** (`xs: List<int> = b.collect()` seeds a return-only `U` from the annotation). Bounds on a method parameter ride the ordinary trait machinery (`fn bigger<U: Comparable>(...)`; a non-`Comparable` argument is E0025). The parameters are erased like every generic — one compiled method serves every instantiation.

**A method's own parameter shadows.** Reusing one of the class's names declares a *new* parameter that hides the outer one for the body's extent, exactly as a local binding hides a global — `Repo::<Todo>.label::<User>()` on a `fn label<T>()` inside `class Repo<T>` answers `User`, because the receiver's turbofish binds the class's `T` and the method's binds its own. The outer parameter is simply unreachable by that name inside the method; where you want both, give them different names (`fn paired<U>` above), which is also what reads better.

A method's own parameter **forwards** into a call-site-typed position exactly as a free function's does — `json.try_parse::<U>` inside a method body is fine (see [Forwarding `T`](#forwarding-t) for the one thing to know about it). One boundary still holds statically: a **trait's** required-method set stays monomorphic — a per-method `<U>` on a trait method is E0058, since the trait is dispatched dynamically and each `impl` would have to agree on the parameter; put the generic method on a concrete type, or make the whole trait generic (`trait T<U> { ... }`).

### Both at once

A **self-less** member of a generic class carrying its own uninferable parameter needs both turbofishes on one call, and takes them:

```noeta
struct Todo { id: int }

class Repo<T> {
    pub fn blank<U>(): string { return type_name::<T>() ~ "/" ~ type_name::<U>() }
}

echo Repo::<Todo>.blank::<int>()    // Todo/int
```

Every other combination has a shorter spelling, which is why this one is the case worth naming. `blank` never reads `self`, so it must be called on the type and the class's `T` can only come from the receiver; its own `U` appears only in the return, so no argument and no annotation can pin it either. With an argument to infer from you write `Repo::<Todo>.describe(1)`; on an instance you write `r.blank::<int>()`, because a value carries its own instantiation already — a receiver turbofish on one is E0058.

The two remain separate concepts checked against separate parameter lists: the turbofish before the `.` is arity-checked against the class's parameters, the one after the member against the method's own. One consequence is worth knowing: a method parameter that **reuses** a class parameter's name does not shadow it — the class's binding wins, and the method's parameter of that name is unreachable. Give it a different name (`fn blank<U>()`, not `fn blank<T>()`) and there is nothing to trip over.

## Forwarding `T`

Inside a generic function's body, `T` is legal wherever a type goes — including as a turbofish argument to another generic, to a call-site-typed native function (`json.try_parse::<T>`), to the name-keyed reflection queries (`type_name::<T>()`, `field_specs_of::<T>()`, `variants_of::<T>()`, `construct::<T>(…)`, `attributes_of::<T>()`, `roles_of::<E>()`), and to `channel::<T>(cap)`:

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

The boundaries. An instantiation the call site cannot pin must be spelled with a turbofish (E0023), and a function that forwards `T` this way is not usable as a bare value (E0058) — call it, or wrap it in a closure.

The remaining boundary is about **what the consumer needs**, not about which parameter it is. The two per-instantiation channels — the receiver's type tag, and the hidden slot the call site fills — both deliver the instantiation's *name*; only the slot additionally delivers a *build recipe*. So a **name** consumer (every reflection query listed above) forwards on either channel, including a generic type's parameter read off `self` in an instance method. A **recipe** consumer (`json.try_parse::<T>`) forwards on the slot alone, and a generic type's parameter in a method body is E0058 there — take the type as the method's own parameter instead. `from_bytes::<T>(blob)` forwards on neither: it needs the element's packed *layout*, which no channel carries.

One more, and it is about **spelling** rather than about what a parameter is. The slots a body forwards through are computed by a pass that runs before checking and reads the source as written, so it registers a forward from a call spelled on a **bare name** — `json.try_parse::<T>(s)`, `f::<T>(x)`, `self.load::<T>(s)`, `store.load::<T>(s)`, `Store.load::<T>(s)`. A receiver that is itself an expression (`self.inner.load::<T>(s)`) names its callee only once checking has typed it, which is too late, and is E0058. Bind the receiver first and the call is spelled on a name again:

```noeta check
use std.json
use std.json.JsonError

struct Order { id: int }

class Store {
    pub tag: string
    pub fn new(): Store { return Store { tag: "s" } }

    pub fn load<T>(text: string): Result<T, JsonError> {
        echo self.tag
        return json.try_parse::<T>(text)
    }
}

class Cache {
    pub inner: Store
    pub fn new(): Cache { return Cache { inner: Store.new() } }

    pub fn get<T>(text: string): Result<T, JsonError> {
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

    pub fn new(label: string): Loader {
        return Loader { label: label }
    }

    pub fn load<T>(text: string): Result<T, JsonError> {
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

The answer is the **qualified** identity, byte-identical to what the concrete `type_name::<Order>()` yields at the same site — the declaring module's path, a `use … as` alias and a rename all followed. That agreement is the whole contract: the string exists to key a name-keyed registry, so a forwarded parameter answering the short name would silently miss every type declared in a module.

It is also what opens the *other* name-keyed queries to a generic body. `field_specs_of::<T>()`, `variants_of::<T>()` and `construct::<T>(…)` key on that same name, so a parameter in their turbofish resolves through the same channel rather than demanding a compile-time constant — the compiler composes `field_specs_of(type_name::<T>())` for you, and the hand-written composition still means exactly the same thing:

```noeta check
struct Order { id: int }

fn field_count<T>(): int {
    return field_specs_of::<T>().len()   // the real schema, per instantiation
}

echo field_count::<Order>()
```

See [Reflection over a type parameter](Attributes-and-Reflection#reflection-over-a-type-parameter).

The two channels are separate and both reach a method body, so a method's own `<U>` and its class's `<K>` can be asked about in the same expression: `U` resolves through the slot the call supplied, `K` off the receiver's type tag. Where a name shadows, the method's own wins — ordinary scoping.

What stays E0058 is a site **no** channel reaches: a nested `fn`'s own parameter, which no call site instantiates, and a class's parameter inside a nested `fn`, which has no receiver to read the tag off. A *composite* turbofish is unaffected either way: `type_name::<List<T>>()` heads at `List` whatever `T` is, so it is a compile-time constant.

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
| `From<Source>` | `from(value: Source): Target` — static | error conversion at `?` — see [Error Handling](Error-Handling#converting-errors-at---impl-fromsource) |
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
        pub fn key(): int {
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
    pub fn new(a: int): Money { return Money { amount: a } }
    impl Add {
        pub fn add(other: Money): Money { return Money { amount: self.amount + other.amount } }
    }
    impl Comparable {
        pub fn compare(other: Money): Ordering { return self.amount.compare(other.amount) }
    }
}
echo (Money.new(3) < Money.new(5))   // true
```

**`Callable` makes an object invocable.** A type implementing `Callable` with a `call` method can be applied like a function — `obj(args)` dispatches to `obj.call(args)`, with the receiver's state in scope. The arity is the method's own (the protocol does not pin it), and the call is arity/argument-checked against the method's signature like any method call. A user type without a `call` method is statically not callable (E0007).

```noeta
class Adder {
    pub base: int
    impl Callable {
        pub fn call(x: int): int { return self.base + x }
    }
}
add10 = Adder { base: 10 }
echo add10(5)     // 15
```

There is also a **standalone** `impl Trait for T { ... }`, which must target a struct, class, or enum **the program declares** — a target no module declares is E0013, a wrong or missing required method is E0015. "The program" is the whole linked program rather than the one file: a module may implement a trait for a sibling module's type, or for a type declared in the entry, so where in your package you put an impl is a matter of layout. Across a *package* boundary it is not free: the [orphan rule](#the-orphan-rule) requires the impl to sit with the trait or with the type (E0070). A standalone impl of a **user** trait may carry method bodies (hoisted onto the target type); an impl of a **built-in** trait must stay an empty-body marker (a body is E0015 — those methods live in the type's own body).

**A standalone impl travels with its type.** Wherever the target type reaches — a sibling module, a consumer of your package, a consumer that never names the type and only ever holds it as a `dyn Trait` — its standalone impls go with it, and every surface that reads trait membership sees them: the `dyn Trait` coercion, trait-method dispatch, the precise `x is dyn Trait` test, a `<T: Trait>` bound, and `traits_of(x)`. Which spelling you choose is a matter of layout, never of reach — an in-body `impl Trait { … }` block and a standalone `impl Trait for T { … }` declare the same thing, and a type that must be written one way to be usable from another module is a bug, not a rule.

**Default methods fall back.** A user trait's method *with* a body is a **default**: an implementor may omit it, and the omitted method falls back to the trait's default body — hoisted onto the implementing type, so it dispatches like any method (a default that mentions `self` is an instance method and may call the trait's required methods; a self-less default needs no receiver and may be called either way — see the receiver rule below). A method the impl provides always overrides its default — and an override is held to the trait's signature exactly like a required method is. A **generic** trait implements at an instantiation — `impl Keyed<int> { … }`, `impl Keyed<string> for Tag { … }`, `@derive(Keyed<string>)` — and its defaults substitute the type parameters through their signatures and bodies before hoisting; a bare `impl Keyed`/`@derive(Keyed)` on a generic trait is an arity error naming the parameters.

**Implementations derive their receiver; contracts may declare it.** A method whose body mentions `self` is an **instance** method: call it on a value, `x.m(…)`. A self-less one that belongs to no trait is a **static** function: call it on the type, `T.m(…)`. Getting that backwards is E0047 either way — a static call has no receiver to bind `self` to, and an instance call on a static function would evaluate a receiver and discard it.

That derivation is the whole story for an implementation, and it is not something you may override: `static` on an inherent method, on a method inside an `impl` block, or on a top-level `fn` is E0015. The body already says it, exactly and visibly, and a modifier there would be a second source of truth that can drift from the first. The one place you may *declare* it is a `trait`'s method contract — see [Declaring a static method](#declaring-a-static-method) below.

A self-less method that a **trait** supplies is the third case, and it accepts **both**. The trait's contract puts it in the instance interface — that is how `dyn Trait` reaches it — while its body needs no receiver, so `T.m(…)` is equally well-defined; both spellings run the same code. This holds identically for an in-body `impl Trait { … }` block, a standalone `impl Trait for T { … }`, and a hoisted default, so how you spell the implementation never changes how you may call it:

```noeta
trait Greeter { fn greet(who: string): string }

struct En {
    id: int
    impl Greeter {
        pub fn greet(who: string): string { return "hi " ~ who }
    }
}
echo En.greet("Ada")             // hi Ada — on the type
echo En { id: 1 }.greet("Bob")   // hi Bob — on a value
```

(`From` is the exception: it declares its `from` **static** — a conversion *builds* a value rather than acting on one — so it stays type-only, and a `from` body that mentions `self` is E0015. It is an ordinary declared-static method; nothing about it is special-cased.)

#### Declaring a static method

**`static fn m(…)` in a trait declaration promises no implementation binds `self`.** It is legal in a trait body only — on a required signature or on one that carries a default — and it makes receiver-ness a term of the contract, alongside arity, parameter types, return type and `async`-ness:

```noeta
trait Codec {
    static fn decode(raw: string): Self   // no implementation may bind `self`
    static fn tag(): string { return "codec" }
    fn encode(): string                   // an ordinary instance method
}
```

Every implementation is held to it. A body that mentions `self` — in a fresh `impl`, in an *override* of a defaulted static method, or in the trait's own default body — is E0015, the same code and the same class of error as the wrong arity.

**Unmarked stays unconstrained.** Leaving the modifier off means exactly what it always meant: implementations derive their own receiver-ness from their bodies, and a self-less one is reachable both ways. What the modifier buys is the section below on bounded type parameters — it is the promise a generic body spends.

On a user trait the modifier **adds** a guarantee rather than withdrawing a spelling: a self-less trait method already answers `x.m(…)` as well as `T.m(…)` (the third case above), and marking it `static` leaves that alone. The built-in `From` is stricter — `x.from(…)` is E0047, because `from` has been type-only since before the modifier existed and narrowing it is the behaviour callers already depend on.

**A bounded type parameter is a receiver too.** `T.m(…)` inside `fn f<T: Trait>` reaches the methods `Trait` declares **`static`**, because a bound is what licenses them and `T` is the type at run time. One compiled body serves every instantiation, so the call resolves the instantiation's name per call and dispatches on it; nothing is monomorphized.

Only those. `T.m(…)` where the bound's trait does not declare `m` static is E0047, reported at the *definition* — a generic body calling `T.m(…)` is promising something every implementor of the bound must keep, and the body is what makes a promise the bound does not carry. The fix the diagnostic names is to write `static` in the trait, which then holds every implementation to it.

The declaration is doing real work here, and an implementation that happens to be self-less is not a substitute for it. Nor is a self-less *default*: a default says what the default does, never what an override does, and only the declaration binds an override. Everything else a type name can do (`T { … }` construction, `T.Variant`, `T` in an annotation) is not licensed by a bound and stays unavailable. Arguments bind positionally here: the dispatch is by name, and a name has no labels to bind with.

```noeta
trait Buildable { static fn make(seed: int): Self }
struct Thing {
    v: int
    impl Buildable { pub fn make(seed: int): Thing { return Thing { v: seed } } }
}
fn build<T: Buildable>(seed: int): T {
    return T.make(seed)     // `Self` is `T`, so this is the declared return
}
echo build::<Thing>(3).v    // 3
```

**The signature is the contract, `async` and `static` included.** An implementation must match the trait's declaration in arity, parameter types, return type, `async`-ness, *and* a declared `static`; a mismatch is E0015. `async` belongs on that list because every receiver form types a call from *some* signature and they must all agree: an `async fn m(): T` is called for a `Future<T>` and a plain `fn m(): T` for a `T`, so a synchronous method satisfying an `async` declaration (or the reverse) would make a bound's and a trait object's typing a promise the implementation does not keep.

**`Self` is the implementing type.** A trait declaration may write `Self` anywhere a type goes — `fn decode(raw: string): Self`, `fn combine(other: Self): int`, `fn spread(): List<Self>` — and it stands for whichever type implements the trait. It is a *declaration* spelling only: the implementation writes the concrete type it is being written for (`fn decode(raw: string): Thing`), and spelling `Self` back in an `impl` is E0013, because at that point the type is known and has a name.

Every way of reaching the method resolves `Self` to the receiver you actually have, so the same signature reads the same through all three of them: on a concrete value it is that value's type, under a `<T: Trait>` bound it is `T`, and on a `dyn Trait` it is `dyn Trait` — the receiver *is* some implementor, and that is as precise as the erasure allows.

```noeta
trait Decodable { fn decode(raw: string): Self }
struct Thing {
    v: int
    impl Decodable {
        pub fn decode(raw: string): Thing { return Thing { v: raw.len() } }
    }
}
fn rebuild<T: Decodable>(seed: T, raw: string): T {
    return seed.decode(raw)     // a `T`, because `Self` is `T` here
}
echo rebuild(Thing { v: 0 }, "hello").v   // 5
```

## Trait objects

A `dyn Trait` parameter or binding accepts any implementor and dispatches on the value's concrete type at run time, while typing statically from the trait's declaration — return type, parameter types, arity, and whether the method is `async`:

```noeta
use std.task.{sleep}

trait Fetcher {
    async fn fetch(url: string): string
}

struct Http {
    impl Fetcher {
        pub async fn fetch(url: string): string {
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
    pub fn new(x: int, y: int): Point { return Point { x: x, y: y } }
}
echo Point.new(1, 2) < Point.new(1, 3)   // true
```

The derivable built-ins are `Equatable`, `Comparable`, `Display`, `Error`, `Clone`, and `Serialize<Json>`; a fully-defaulted user trait derives too, and the `member:`/`via:` bindings bridge or delegate what a plain derive cannot reach. The full story — the derivable table, user-trait derives, bridging, delegation, native recipes, field constraints, and conditional generic derives — lives on [Derives](Derives).

## Coherence

Coherence has two halves: **uniqueness** — at most one implementation per (type, trait) pair — and the **orphan rule** — who is allowed to write one.

### Uniqueness

Each type has **at most one** implementation of a given trait, across `@derive(T)`, an in-body `impl T { }`, and a standalone `impl T for Type { }`. A duplicate or competing impl is E0027 — including two *different modules* that each implement one trait for one type, which is a conflict like any other and is reported rather than silently resolved in someone's favor. The diagnostic labels **both** sites, each rendered against its own file, since the two are routinely in different modules.

Uniqueness is always decidable here, and that is a property of the language: Noeta links **one whole program** at a time, so every module and every dependency is resolved into a single program before it is checked, and there is no separately-compiled unit that could hold an implementation this one cannot see.

The same rule holds one level down, over method **names**: two traits a type implements may not each hand it a default body for the same method. A method table has one slot per name — there is no overloading — so two inherited defaults are two bodies for one slot, and nothing in the source says which one is meant; that is E0027 too, labelling both bindings. Resolve it the way an implementor resolves any default it does not want: **provide the method**, which overrides every default and, where both traits' signatures accept it, satisfies both — or implement one of the traits fewer. Two traits merely *naming* the same method is not the conflict; only two defaults contending for a slot the type leaves empty is.

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
