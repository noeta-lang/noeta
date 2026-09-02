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

Any generic function can be instantiated explicitly with `f::<T, ...>(args)`. The type arguments bind to the declared type parameters **in order**, one per parameter. An arity mismatch is E0058, as is a turbofish on a non-generic function.

An explicit binding **wins** over argument inference. An argument that disagrees with it fails assignability against the substituted parameter, E0007.

```noeta
fn empty<T>(): List<T> { return [] }
fn pick<T>(a: T, b: T, first: bool): T { if first { return a } return b }

xs: List<int> = empty::<int>()      // T appears only in the return — turbofish carries it
echo pick::<string>("x", "y", false)
```

The expected type at an annotated binding can carry the instantiation too. `r: List<int> = empty()` infers `T = int` from the **return position**: the bidirectional checker seeds `T` from the expectation, and the arguments fill what it leaves open.

Inference flows through `?` and `??` as well, with no turbofish. `o: Order = load(text)?` seeds `T = Order` by inverting the `Result` wrapper at the checked `Try`, and `o: Order = load(text) ?? fallback` seeds it through the coalesce.

A **container literal written inline at the call** instantiates from what it holds, exactly as the same value bound to a local first does. `f([x])` and `ys = [x]; f(ys)` pin `T` identically, for a list, set, map, `.{ … }` and any nesting of them.

An **empty** literal holds nothing to read an instantiation off, so a parameter only it could pin stays open and is E0023. Spell it (`f::<int>([])`) or annotate the value first.

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

`Type::<Args>.method(args)` works for **any** static function. Noeta has no constructor concept, so `Repo::<Todo>.open(dsn)` and `Repo::<Todo>.from_dsn(dsn)` read exactly as `new` does.

The type arguments bind to the type's declared parameters in order, arity-checked as E0058, as is a turbofish on a non-generic type. They are the *class's* parameters and only those: a method's own `<U>` is instantiated on the method, so `Repo.new::<Todo>` is an error, since `new` declares no type parameters of its own. Where both are generic, both are spelled; see [Both at once](#both-at-once).

The `::` is required: `Repo<Todo>.new(…)` is a parse error, because a bare `<` after an identifier is ambiguous with less-than in expression position. That is the same reason every other type argument in the language is written with a turbofish.

**When to reach for it.** Prefer an annotation or a declared type where the position already has one, which documents the value where a reader looks for it. Four positions supply the instantiation that way:

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

Reach for the call-site turbofish where none of them does: the value is echoed straight out, passed to a `dyn` parameter, or bound with no annotation you want to write. A construction that records no instantiation at all is a check-time E0058 whose help names this spelling.

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

`Repo::<Todo>.new("todos")` is a self-sufficient expression of type `Repo<Todo>`, exactly as `5` is an `int`, so the mismatch is reported by the same rule and at the same span as `n: int = "s"`.

## Generic methods

A method may declare **its own** type parameters, instantiated per call and independent of the class's parameters, which the receiver pins. The two substitutions compose, so a `Box<int>` receiver with `paired::<string>(…)` binds `T = int` and `U = string`.

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

All three instantiation paths a free function has apply here, and mean the same things:

- **argument inference**, so `b.choose(99, true)` infers `U = int`.
- the **member turbofish** `recv.m::<U>(args)`, which carries the method's own parameters alone and is arity-checked against them (E0058).
- **expected-type seeding**, so `xs: List<int> = b.collect()` seeds a return-only `U` from the annotation.

Bounds on a method parameter ride the ordinary trait machinery (`fn bigger<U: Comparable>(...)`; a non-`Comparable` argument is E0025).

**A method's own parameter shadows.** Reusing one of the class's names declares a *new* parameter that hides the outer one for the body's extent. `Repo::<Todo>.label::<User>()` on a `fn label<T>()` inside `class Repo<T>` answers `User`, because the receiver's turbofish binds the class's `T` and the method's binds its own. The outer parameter is unreachable by that name inside the method, so give the two different names where you want both.

**E0075 is a warning**, raised at the inner declaration. The program compiles and means what it says, and a reader cannot tell the two `T`s apart by sight, so the diagnostic asks you to rename one.

A method's own parameter **forwards** into a call-site-typed position exactly as a free function's does, so `json.try_parse::<U>` inside a method body is fine. See [Forwarding `T`](#forwarding-t).

A **trait's** required-method set stays monomorphic. A per-method `<U>` on a trait method is E0058, since the trait is dispatched dynamically and each `impl` would have to agree on the parameter. Put the generic method on a concrete type, or make the whole trait generic (`trait T<U> { ... }`).

### Both at once

A **self-less** member of a generic class carrying its own uninferable parameter needs both turbofishes on one call, and takes them:

```noeta
struct Todo { id: int }

class Repo<T> {
    pub items: List<T>
    pub fn make(xs: List<T>): Repo<T> { return Repo { items: xs } }
    pub fn blank<U>(): string { return type_name::<T>() ~ "/" ~ type_name::<U>() }
    pub fn tagged<U>(): string { return type_name::<U>() ~ "@${self.items.len()}" }
}

echo Repo::<Todo>.blank::<int>()          // Todo/int   — static: both turbofishes

r = Repo::<Todo>.make([])
echo r.tagged::<int>()                    // int@0      — self-taking: the member turbofish alone
```

Every other combination has a shorter spelling. `blank` never reads `self`, so it must be called on the type and the class's `T` can come only from the receiver. Its own `U` appears only in the return, so no argument and no annotation pins it either. Where there is an argument to infer from you write `Repo::<Todo>.describe(1)`.

A `self`-taking method needs the member turbofish alone, `r.tagged::<int>()`, because a value carries its own instantiation, and a receiver turbofish on a value is E0058. That shorter spelling belongs to the self-taking case: `r.blank::<int>()` is E0047, since `blank` is a static function of `Repo` and a value is the wrong receiver for it.

The two are checked against separate parameter lists. The turbofish before the `.` is arity-checked against the class's parameters, and the one after the member against the method's own. Where a method parameter **reuses** a class parameter's name, the method's own wins by ordinary shadowing, reported as the E0075 warning ([Generic methods](#generic-methods)). Give it a different name, `fn blank<U>()`, and there is nothing to read twice.

## Forwarding `T`

Inside a generic function's body, `T` is legal wherever a type goes, including as a turbofish argument to another generic, to a call-site-typed native function (`json.try_parse::<T>`), to the name-keyed reflection queries (`type_name::<T>()`, `field_specs_of::<T>()`, `variants_of::<T>()`, `construct::<T>(…)`, `attributes_of::<T>()`, `roles_of::<E>()`), and to `channel::<T>(cap)`:

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

Forwarding works from a **top-level generic function**, from a **nested `fn`** inside one, and from a **generic method**, where the parameter is the method's own `<T>` rather than the class's. A **composite** parameter forwards too, so `List<T>` and `Box<T>` reach a `::<...>` position.

An instantiation the call site cannot pin must be spelled with a turbofish (E0023). A function that forwards `T` this way is E0058 as a bare value, so call it or wrap it in a closure.

What forwards depends on what the consumer needs. Two per-instantiation channels exist, the receiver's type tag and the hidden slot the call site fills. Both deliver the instantiation's *name*, and the slot alone also delivers a *build recipe*.

| Consumer | What it needs | Where it forwards |
|---|---|---|
| every reflection query listed above | the instantiation's name | either channel, including a generic type's parameter read off `self` in an instance method |
| `json.try_parse::<T>` | a build recipe | the slot alone; a generic type's parameter in a method body is E0058, so take the type as the method's own parameter |
| `from_bytes::<T>(blob)` | the element's packed *layout* | neither channel carries one |

**The callee must be spelled on a bare name**, since a forward is registered from the source as written, before checking. `json.try_parse::<T>(s)`, `f::<T>(x)`, `self.load::<T>(s)`, `store.load::<T>(s)` and `Store.load::<T>(s)` all register one. A receiver that is itself an expression, such as `self.inner.load::<T>(s)`, is E0058. Bind the receiver first and the call is spelled on a name again:

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

**One boundary is enforced at run time.** A forwarding method must be called by a name the checker can resolve a receiver type for, since that is what pins the instantiation. Four dynamic ways in carry none, a `dyn` receiver, a bound handle (`v.m`), an unbound handle (`T.m`), and `invoke(v, "m", args)`. A forwarding method reached through one of them **aborts**, naming the callee. `type_name::<T>()` makes the same judgment on a value built at no known instantiation.

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

`type_name::<T>()` forwards too. It wants the instantiation's **name** and nothing else, so it rides the same hidden slot with no recipe involved, and it serves an instantiation that has no recipe, a `class` for instance.

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

The answer is the **qualified** identity, byte-identical to what the concrete `type_name::<Order>()` yields at the same site, following the declaring module's path, a `use … as` alias and a rename alike. The string exists to key a name-keyed registry, and that agreement is what makes a forwarded parameter usable as the key.

The *other* name-keyed queries reach a generic body through that same name. `field_specs_of::<T>()`, `variants_of::<T>()` and `construct::<T>(…)` key on it, so a parameter in their turbofish resolves through the same channel. The compiler composes `field_specs_of(type_name::<T>())` for you, and writing that composition by hand means the same thing:

```noeta check
struct Order { id: int }

fn field_count<T>(): int {
    return field_specs_of::<T>().len()   // the real schema, per instantiation
}

echo field_count::<Order>()
```

See [Reflection over a type parameter](Attributes-and-Reflection#reflection-over-a-type-parameter).

Both channels reach a method body, so a method's own `<U>` and its class's `<K>` can be asked about in the same expression. `U` resolves through the slot the call supplied, and `K` off the receiver's type tag. Where a name shadows, the method's own wins by ordinary scoping.

A site **no** channel reaches is E0058: a nested `fn`'s own parameter, which no call site instantiates, and a class's parameter inside a nested `fn`, which has no receiver to read the tag off. A *composite* turbofish stands apart from all of this, since `type_name::<List<T>>()` heads at `List` whatever `T` is and is a compile-time constant.

## Generic functions as values

A generic function referenced as a **value** in an expected-type position instantiates against the expectation and carries the precise monomorphic function type, with no turbofish needed:

```noeta
fn wrap<T>(x: T): List<T> { return [x] }

ys = [1, 2, 3].map(wrap)             // List<List<int>> — wrap instantiates as (int) -> List<int>
f: (string) -> List<string> = wrap   // and here as (string) -> List<string>
```

Declared bounds ride along, so `g: (Box, Box) -> Box = biggest` is E0025 when `Box` is not `Comparable`. A bare, expectation-free binding (`f = wrap`) keeps the gradual value: the erased signature, parameters `dyn`, calls deferred per position. The runtime value is the same erased function either way, and the static judgment is what changes.

## The built-in traits

The traits below are **built into the language**, a fixed set an `impl` or `@derive(...)` may name. Naming a trait that is neither one of these nor [declared in the program](#implementing-a-trait) is E0014. They are what operators and protocols dispatch through, so `==` knows which method answers it. A program declares its own traits alongside them.

| Trait | Method | `dyn` | Lights up |
|---|---|---|---|
| `Equatable` | `eq(other): bool` | yes | `==` `!=` |
| `Comparable` | `compare(other): Ordering` | yes | `< <= > >=` |
| `Display` | `to_string(): string` | yes | `echo`, `${…}` |
| `Error` | `message(): string` | yes | the idiomatic `Err` payload ([Error Handling](Error-Handling)) |
| `Validate` | `validate(): Result<void, E>` | yes | a data-boundary invariant, run at typed decode ([Validation](Validation)) |
| `From<Source>` | `from(value: Source): Target`, static | no | error conversion at `?` ([Error Handling](Error-Handling#converting-errors-at---impl-fromsource)) |
| `To<Target>` | `to(): Target` | no | the same conversion declared on the **source**, for a target you do not own ([Error Handling](Error-Handling#converting-into-a-type-you-do-not-own--impl-totarget)) |
| `Add` | `add(other): T` | yes | `+` |
| `Sub` | `sub(other): T` | yes | `-` |
| `Mul` | `mul(other): T` | yes | `*` |
| `Div` | `div(other): T` | yes | `/` |
| `Concat` | `concat(other): T` | yes | `~` |
| `TryAdd` | `try_add(other): Result<T, E>` | yes | the explicit `a.try_add(b)?` |
| `Index` | `get(i): T` | yes | `a[i]` |
| `Length` | `len(): int` | yes | `x.len()` on a `<T: Length>` parameter |
| `Iterable` | `iter(): List<T>` | yes | `for x in o` |
| `Callable` | `call(...)`, any arity | yes | `obj(args)` |
| `Members` | `get(name): T` | yes | a `<T: Members>` bound: look a member up by name |
| `DynamicCall` | `call(name, args): T` | yes | a `<T: DynamicCall>` bound: invoke by name |
| `Serialize` | none | no | `@derive(Serialize<Json>)` synthesizes the encoder ([Derives](Derives)) |
| `Deserialize` | none | no | `@derive(Deserialize<Json>)` registers the decoder ([Derives](Derives)) |
| `Clone` | none | no | membership only: the type satisfies a `<T: Clone>` bound, and structs already copy on assignment |

The **`dyn` column** says whether `dyn Trait` names a type ([Trait objects](#trait-objects)). A trait object is a value plus a method to call on it, and five traits are missing one of those pieces. `Clone`, `Serialize` and `Deserialize` impose no method to dispatch. `From`'s `from` is `static`, called on the type, so there is no receiver to erase. `From`, `To`, `Serialize` and `Deserialize` each take a type argument that `dyn Name` has no syntax to carry, since a `dyn To` would not say *to what*.

Writing one of the five is E0014 where it is written. Use a `<T: Trait>` bound, or the concrete type.

### Comparison and ordering

`Ordering` is a namable built-in enum (`Ordering.Less` / `Equal` / `Greater`); calling `.compare()` on a primitive returns it.

**`x.compare(y)` is available exactly where `x < y` is**, since one trait answers both doors.

Numbers, strings and bools order. So do `?T` and `Result<T, E>` when their payloads do, variant first and then payload. So does any type carrying `@derive(Comparable)` or an `impl Comparable`, and any native type that declares one.

A receiver with no ordering is refused at `.compare()` where it is written, with the same E0007 `<` gives it. That covers a `List`, a `Map`, a `Set`, a tuple, a type you declared without the trait, and a native type such as `Duration` that declares none. A `dyn` value and an unbounded type parameter keep the call, like every other member on them.

### Ordering your own type

A type that implements `Comparable` decides the order of its own values at **every door a program can observe one through**: the operators `< <= > >=`, and `.sorted()`, `.min()` and `.max()` over a list of it. One comparator serves all of them, so `xs.min()` is `xs.sorted().first()` for any type, whatever its `compare` says.

```noeta
struct Rev { n: int }
impl Comparable for Rev {
    pub fn compare(other: Rev): Ordering { return other.n.compare(self.n) }
}

xs = [Rev { n: 1 }, Rev { n: 3 }, Rev { n: 2 }]
echo xs.sorted()      // [Rev {n: 3}, Rev {n: 2}, Rev {n: 1}]
echo xs.min()         // some(Rev {n: 3})
```

**A `compare` orders its own type's values.** `@derive(Comparable)` is *field-wise structural* ordering, so a derived type holding a field whose type writes its own `compare` still compares that field structurally. To get the field's order, write the outer type's `compare` and call it: `return self.field.compare(other.field)`. `<` and `.sorted()` then answer alike about the outer type.

**A `set` and a `map` place a value by its own fields rather than by its `compare`.** Those are *identity* orders: a value is placed at one moment and looked for at another, so the placement stays a pure function of the value's own fields. A set of a type that orders descending therefore iterates ascending, `.sorted()` over the same elements gives the type's own order, and every member put in is found.

A `compare` may be any relation, and nothing checks it for totality. One that reports both `a < b` and `b < a` yields some permutation of the input rather than a sorted list. Both engines produce the same permutation, and neither aborts.

### Concatenation and addition

**`~` produces what the left operand's `concat` produces.** The operator has two jobs, display-concatenating any two operands into a `string` and overloading through `impl Concat`, and the **left** operand decides which. A value whose type implements `Concat` is asked for its `concat`, and the result is whatever that method returns. A `string` or a list on the left display-concatenates rather than dispatching, so `"total: " ~ price` renders `price` into text as it reads.

**`TryAdd` is a method rather than an operator.** Bare `+` is reserved for the infallible `Add`, so a type whose only addition is `TryAdd` rejects `a + b` with E0007 and the addition is written out as `a.try_add(b)?`. An expression built from `+` therefore carries no failure path, and the one that can fail says so at the call site.

### Programming against `Members` and `DynamicCall`

`Members` requires `get(name)` and `DynamicCall` requires `call(name, args)`, and implementing one buys trait membership alone, meaning a `<T: Members>` or `<T: DynamicCall>` bound (E0025 without the impl) and a `traits_of` entry. They name the capability "this type resolves a member, or a call, from a name at runtime" so a framework can require it. The methods are ordinary methods, callable as `x.get(name)` and `x.call(name, args)` with or without the impl.

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

- **Body-side requirement.** Using `>` requires `Comparable` and `+` requires `Add`; on an *unbounded* `T` that operation is E0025 at the definition. The requirement follows `T` into a **collection** of it, so `xs.sorted()`, `xs.max()` and `xs.iter().min()` each order `T`s and each needs the same `Comparable`.
- **Call-site check.** Instantiating with a type that does not satisfy the bound is E0025. The first argument pins `T`, and later arguments are checked against that substitution (E0007 on mismatch).
- An unknown bound name is E0014.

**Instantiated bounds.** A bound on a *generic user trait* may demand a specific instantiation. `<T: Keyed<int>>` is satisfied by a type with an `impl Keyed<int>`, and an `impl Keyed<string>` fails it with E0025 naming `Keyed<int>`. A bare `<T: Keyed>` accepts any instantiation. A bound argument may name a sibling parameter, so `<K, T: Keyed<K>>` ties the two together with `K` pinned by the call's arguments:

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

An instantiated bound must match the trait's arity, so `T: Keyed<int, string>` on a one-parameter trait is E0014. Built-in traits take no bound arguments. The conversion traits carry their type argument on the *impl* (`impl From<JsonError> { … }`, `impl To<ServiceError> for DiskError { … }`), and neither is usable as an instantiated bound.

The bound also types the **body**. On a `T`-typed value, a method the bound's trait declares resolves at the bound's instantiation: under `<T: Keyed<int>>`, `item.key()` is an `int` and `item.same(x)` demands an `int`, so a wrong argument, return, or arity is E0007 at the definition, before any call site exists. A method no bound declares stays leniently deferred.

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

**`Equatable` and `Comparable` answer to the operator, so their return types are fixed.** `eq`'s answer *is* the result of `==`, and `compare`'s is read as a direction, so they must be declared `bool` and `Ordering` respectively. A mismatch is E0015, and the return type is required here rather than optional. Every other trait leaves the return to the implementor, so `x.len()` is typed from the signature `Length`'s implementor wrote and every reader agrees with it.

**`Callable` makes an object invocable.** A type implementing `Callable` with a `call` method can be applied like a function: `obj(args)` dispatches to `obj.call(args)`, with the receiver's state in scope. The arity is the method's own, and the call is arity- and argument-checked against the method's signature like any method call. Applying a user type that has no `call` method is E0007 at check time.

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

### Standalone impls

A **standalone** `impl Trait for T { ... }` targets a struct, class, or enum **the program declares**. A target no module declares is E0013, and a wrong or missing required method is E0015.

"The program" is the whole linked program rather than one file. A module may implement a trait for a sibling module's type, or for a type declared in the entry, so where in a package an impl sits is a matter of layout. Across a *package* boundary the [orphan rule](#the-orphan-rule) requires the impl to sit with the trait or with the type (E0070).

It carries method bodies for any trait, built-in or user-declared. A bodiless `struct` has nowhere inside it to write one, so this is where its operators and protocols go:

```noeta
struct Rev { n: int }

impl Comparable for Rev {
    pub fn compare(other: Rev): Ordering { return other.n.compare(self.n) }   // descending
}

impl Display for Rev {
    pub fn to_string(): string { return "Rev#${self.n}" }
}

echo (Rev { n: 1 } < Rev { n: 2 })                                // false — `Rev` orders the other way
echo [Rev { n: 1 }, Rev { n: 3 }].sorted()                        // [Rev {n: 3}, Rev {n: 1}]
```

An empty body belongs to a **marker** trait, one with no required method such as `Clone` or `Serialize`, where the impl declares a capability. An `impl` names the trait bare: the format argument in `@derive(Serialize<Json>)` belongs to the *derive*, which synthesizes an encoder for that format, and `impl Serialize<Json>` is E0015. Write `impl Serialize for S { }`.

**A standalone impl travels with its type.** Its impls go wherever the target type reaches: a sibling module, a consumer of your package, a consumer that holds it only as a `dyn Trait`. Every surface that reads trait membership sees them, meaning the `dyn Trait` coercion, trait-method dispatch, the precise `x is dyn Trait` test, a `<T: Trait>` bound, and `traits_of(x)`.

An in-body `impl Trait { … }` block and a standalone `impl Trait for T { … }` declare the same thing and reach equally far, so the spelling is a matter of layout.

### Default methods

**Default methods fall back.** A user trait's method *with* a body is a **default**. An implementor may omit it, and the omitted method falls back to the trait's default body, hoisted onto the implementing type so that it dispatches like any method. A default that mentions `self` is an instance method and may call the trait's required methods. A self-less default needs no receiver and may be called either way, by the receiver rule below.

A method the impl provides overrides its default, and an override is held to the trait's signature exactly like a required method is.

A **generic** trait implements at an instantiation: `impl Keyed<int> { … }`, `impl Keyed<string> for Tag { … }`, `@derive(Keyed<string>)`. Its defaults substitute the type parameters through their signatures and bodies before hoisting. A bare `impl Keyed` or `@derive(Keyed)` on a generic trait is an arity error naming the parameters.

### Instance and static receivers

**Implementations derive their receiver; contracts may declare it.** A method whose body mentions `self` is an **instance** method, called on a value as `x.m(…)`. A self-less one is a **static** function, called on the type as `T.m(…)`, unless a trait's interface supplies it and the trait left it undeclared, which is the third case below.

Getting that backwards is E0047 either way, because a static call has no receiver to bind `self` to and an instance call on a static function would evaluate a receiver and then discard it.

An implementation's receiver comes from its body alone, and `static` on an inherent method, on a method inside an `impl` block, or on a top-level `fn` is E0015. The one place you may *declare* it is a `trait`'s method contract, [below](#declaring-a-static-method).

The third case is a self-less method a **trait** supplies where the trait left `static` off, and it accepts **both** spellings. The trait's contract puts it in the instance interface, which is how `dyn Trait` reaches it, and its body needs no receiver, so `T.m(…)` is equally well-defined. Both spellings run the same code, for an in-body `impl Trait { … }` block, a standalone `impl Trait for T { … }`, and a hoisted default alike:

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

`From` follows the same rule as an ordinary declared-static method. It declares its `from` **static**, since a conversion builds a value rather than acting on one, so `from` is type-only on a concrete type and a `from` body that mentions `self` is E0015.

#### Declaring a static method

**`static fn m(…)` in a trait declaration promises no implementation binds `self`.** It is legal in a trait body alone, on a required signature or on one that carries a default, and it makes receiver-ness a term of the contract alongside arity, parameter types, return type and `async`-ness:

```noeta
trait Codec {
    static fn decode(raw: string): Self   // no implementation may bind `self`
    static fn tag(): string { return "codec" }
    fn encode(): string                   // an ordinary instance method
}
```

Every implementation is held to it. A body that mentions `self` is E0015, in a fresh `impl`, in an *override* of a defaulted static method, and in the trait's own default body alike, the same code a wrong arity reports.

**Unmarked stays unconstrained.** With the modifier off, implementations derive their own receiver-ness from their bodies, and a self-less one is reachable both ways.

**A declared-`static` method is type-only on a concrete type.** `T.m(…)` is the call, and `x.m(…)` on a value is E0047, as it is for an inherent static function and for the same reason: both spellings reach the same prototype, so nothing binds the receiver and it would be evaluated and then discarded.

That is what the modifier withdraws. An undeclared self-less trait method takes either spelling, because the trait put it in the instance interface; a declared one carries a contract saying the receiver is not part of the method's meaning. The diagnostic names the trait and points at the `static fn` line, since there is no self-less body in front of you to explain the refusal.

#### Reaching a declared-static method through `dyn` and through a bound

**`dyn Trait` reaches it through instance syntax.** On a concrete type the receiver is discarded, because the type name has already selected the code. A trait object has no type name to call on, so the receiver is what chooses which implementor runs. It *is* the dispatch, and it is never bound to `self`.

```noeta
trait Codec {
    static fn tag(): string
    fn encode(): string
}
struct Blob {
    v: string
    impl Codec {
        pub fn tag(): string { return "blob" }
        pub fn encode(): string { return self.v }
    }
}
fn label(c: dyn Codec): string { return c.tag() }

echo Blob.tag()                 // blob — on the type
echo label(Blob { v: "x" })     // blob — through `dyn Codec`, the receiver dispatching
```

The one spelling that is refused, on the same declarations:

```noeta error
trait Codec { static fn tag(): string }
struct Blob {
    v: string
    impl Codec { pub fn tag(): string { return "blob" } }
}

echo Blob { v: "x" }.tag()      // E0047 — on a value of a concrete type
```

**A bounded type parameter is a receiver too.** `T.m(…)` inside `fn f<T: Trait>` reaches the methods `Trait` declares **`static`**, because a bound is what licenses them and `T` is the type at run time. One compiled body serves every instantiation, so the call resolves the instantiation's name per call and dispatches on it, with no monomorphization.

Those methods and no others. `T.m(…)` where the bound's trait does not declare `m` static is E0047, reported at the *definition*: a generic body calling `T.m(…)` promises something every implementor of the bound must keep. The fix the diagnostic names is to write `static` in the trait, which then holds every implementation to it.

Only the declaration licenses the call. A self-less implementation does not, and neither does a self-less *default*, since a default says what the default does and only the declaration binds an override. A bound licenses nothing else a type name can do, so `T { … }` construction, `T.Variant` and `T` in an annotation stay unavailable. Arguments bind positionally, because the dispatch is by name and a name has no labels to bind with.

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

#### The signature is the contract

An implementation must match the trait's declaration in arity, parameter types, return type, `async`-ness, *and* a declared `static`. A mismatch is E0015. `async` belongs on that list because every receiver form types a call from the declaration: an `async fn m(): T` is called for a `Future<T>` and a plain `fn m(): T` for a `T`, so a bound and a trait object type the call from whichever the trait declares.

**`Self` is the implementing type.** A trait declaration may write `Self` anywhere a type goes (`fn decode(raw: string): Self`, `fn combine(other: Self): int`, `fn spread(): List<Self>`), and it stands for whichever type implements the trait. An implementation may write `Self` back, where it means the type being implemented for, or spell that type out. The two say the same thing, and the conformance check reads them the same way. In a trait's own **default body** `Self` is `dyn <the trait>`, since there the implementor is known only to be one, which is what `self` is bound to.

`Self` works the same way in any type body, a trait's included. See [`Self` — the type in hand](Structs-Classes-and-Enums#self--the-type-in-hand).

Every way of reaching the method resolves `Self` to the receiver in hand, so the same signature reads the same through all three: on a concrete value it is that value's type, under a `<T: Trait>` bound it is `T`, and on a `dyn Trait` it is `dyn Trait`, which is as precise as the erasure allows.

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

A `dyn Trait` parameter or binding accepts any implementor and dispatches on the value's concrete type at run time, while typing statically from the trait's declaration: return type, parameter types, arity, and whether the method is `async`.

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

`dyn` on a **generic** trait erases its parameters. There is no `dyn Trait<...>` surface form, so the parameters instantiate permissively to `dyn` and those positions defer to run time, as they do under a bare `<T: Store>` bound. Name the instantiation with a bound (`<S: Store<int>>`) when you need them typed.

Every `trait` a program declares has a trait object. Among the [built-ins](#the-built-in-traits), the seventeen of the twenty-two carrying a `yes` in the `dyn` column do. `dyn Clone`, `dyn Serialize`, `dyn Deserialize`, `dyn From` and `dyn To` are E0014.

### Trait objects as type arguments

`dyn Trait` is an ordinary type, so it instantiates a generic: `Box<dyn Speak>`, `List<dyn Speak>`, `Map<string, dyn Speak>`, and any generic you declare yourself.

**Build the value where the wider type is stated, and every implementor fits.** A checked position hands its type arguments to the literal, which instantiates at `dyn Speak` and widens each implementor into the field. The checked positions are an annotated binding, a declared parameter, a declared return, another type's declared field, a container element, and a call-site turbofish.

```noeta
trait Speak { fn speak(): string }
struct Dog { impl Speak { pub fn speak(): string { return "woof" } } }
struct Cat { impl Speak { pub fn speak(): string { return "meow" } } }

class Box<T> {
    pub v: T
    pub fn new(v: T): Box<T> { return Box { v: v } }
}

fn heard(b: Box<dyn Speak>): string { return b.v.speak() }

pen: Box<dyn Speak> = Box { v: Dog {} }     // the annotation states `T`
echo pen.v.speak()                          // woof
echo heard(Box { v: Cat {} })               // meow — so does a parameter
echo Box::<dyn Speak>.new(Dog {}).v.speak() // woof — and so does a turbofish
```

This is the spelling to reach for, and it works for every declaration.

#### Widening a value you already hold

**Reading an existing value at a wider argument**, handing a `Box<Dog>` you already hold to something expecting a `Box<dyn Speak>`, depends on where the declaration puts its type parameter. It is allowed where the widened view has no way to put a `dyn Speak` back into the value, which is the case for a parameter that only comes *out*. One rule governs every wider argument, so `Box<Dog>` reads as a `Box<dyn>` and a `Box<Struct>` where it reads as a `Box<dyn Speak>`:
```noeta
trait Speak { fn speak(): string }
struct Dog { impl Speak { pub fn speak(): string { return "woof" } } }

class Reader<T> {
    pub v: T
    pub fn get(): T { return self.v }
}

kennel: Reader<Dog> = Reader { v: Dog {} }
speakers: Reader<dyn Speak> = kennel        // `T` is read-only in `Reader`
echo speakers.get().speak()                 // woof

anything: Reader<dyn> = kennel              // the open `dyn`, by the same rule
echo anything.get().speak()                 // woof

pack: List<Dog> = [Dog {}]
chorus: List<dyn Speak> = pack              // and the built-in containers are the same rule
echo chorus[0].speak()
```

Three occurrences of the parameter close that door, and each one is a way the widened view could hand a `Cat` to code that was checked believing it holds a `Dog`:

- a **`mut` field of a `class`**. A `class` has reference identity, so the widened view *is* the original value, and a store through it is a store into the original. A `struct` is exempt: a struct field-set rebinds the binding rather than writing through a shared object, so the store lands in the widened copy alone.
- a **method parameter** of the parameter's type, in any kind. `fn matches(other: T)` is checked believing `other` is a `Dog`, so calling it through a widened receiver would hand it a `Cat`. A field of function type (`f: (T) -> string`) is the same occurrence, reached through a field.
- reaching either of the above **through another generic type**. A `struct` that holds a `class` shares that class with its copies, so `struct Owner<T> { slot: Slot<T> }` is as restricted as `Slot` is.

Where the widening is refused, E0007 names the occurrence that forced it and points at building the value at the wider type instead:

```noeta error
trait Speak { fn speak(): string }
struct Dog { impl Speak { pub fn speak(): string { return "woof" } } }

class Slot<T> { pub mut v: T }

kennel: Slot<Dog> = Slot { v: Dog {} }
wide: Slot<dyn Speak> = kennel   // E0007: `Slot` stores it in the `mut` field `v`
open: Slot<dyn> = kennel         // E0007: the same occurrence, the same refusal
echo wide.v.speak()
```

The rule reads a declaration. A generic type declared by a [native package](Native-Extensions) has none, and is not widened.

## `@derive` — synthesized implementations

`@derive(...)` generates trait impls from a type's shape. It is a *codegen* directive, distinct from the `#[...]` data attributes on [Attributes & Reflection](Attributes-and-Reflection):

```noeta check
@derive(Equatable, Comparable, Display, Clone)
class Point {
    x: int
    y: int
    pub fn new(x: int, y: int): Point { return Point { x: x, y: y } }
}
echo Point.new(1, 2) < Point.new(1, 3)   // true
```

The built-ins a bare `@derive` synthesizes are `Equatable`, `Comparable`, `Display`, `Error`, `Clone`, `Serialize<Json>` and `Deserialize<Json>`. A fully-defaulted user trait derives too, and the `member:` and `via:` bindings bridge or delegate what a plain derive cannot reach. [Derives](Derives) carries the recipe table, user-trait derives, bridging, delegation, native recipes, field constraints, and conditional generic derives.

## Coherence

Coherence has two halves. **Uniqueness** allows at most one implementation per (type, trait) pair, and the **orphan rule** says who may write one.

### Uniqueness

Each type has **at most one** implementation of a given trait, across `@derive(T)`, an in-body `impl T { }`, and a standalone `impl T for Type { }`. A duplicate or competing impl is E0027, including two *different modules* that each implement one trait for one type. The diagnostic labels **both** sites, each rendered against its own file, since the two are routinely in different modules.

A conversion counts per **counterpart type** rather than per trait. `impl From<HttpError>` beside `impl From<JsonError>` declares two conversions into one target, and a repeated counterpart is the conflict (see [Converting errors at `?`](Error-Handling#converting-errors-at---impl-fromsource)). Every site that reaches a conversion carries the source type, so it says which conversion it means. A second `impl Cache<int>` beside `impl Cache<string>` would hand the type two `get`s with nothing at the call site to choose between them, and stays E0027.

The two conversion spellings state one relation, so `impl From<A>` on `B` beside `impl To<B>` for `A` is that same conflict. See [Converting into a type you do not own](Error-Handling#converting-into-a-type-you-do-not-own--impl-totarget).

Uniqueness is always decidable, since Noeta links **one whole program** at a time and the check sees every implementation in it.

The same rule holds one level down, over method **names**. Two traits a type implements may not each hand it a default body for the same method. A method table has one slot per name, with no overloading, so two inherited defaults are two bodies for one slot and nothing in the source says which is meant. That is E0027, labeling both bindings.

Two traits that merely *name* the same method sit together fine, the conflict being two defaults contending for a slot the type leaves empty. Resolve it by **providing the method**, which overrides every default and, where both signatures accept it, satisfies both.

### The orphan rule

An `impl Trait for Type` must live in the **same package** as the trait **or** as the type. A package that declares neither is E0070.

This is Rust's orphan rule with *crate* read as *package*. It keeps a behavior attached to a package the application names. Without it, a package deep in a dependency graph could implement one vendor's trait for another vendor's type, and the behavior would appear in an application that imports both and names the implementing package nowhere:

```noeta ignore
// third.glue — depends on vendor.a and vendor.b, and is written by neither
impl Speaks for Thing { pub fn speak(): string { return "glue says ${self.id}" } }
```
```noeta ignore
// your application, which imports vendor.a and vendor.b and never mentions third.glue
t = Thing.new(7)
echo t is dyn Speaks   // true — from a package you did not write down
echo t.speak()         // "glue says 7"
```

Two such packages in one graph collide as an E0027 the end user **cannot fix**: they own neither implementation, so the only escape is dropping a dependency. Attaching behavior to a foreign type is what the newtype below is for.

What the rule does **not** restrict:

- **Cross-module impls inside one package.** The boundary is the package, not the file. A module may implement a trait for a type declared in a sibling module, or in the entry, exactly as before.
- **`@derive(Trait)` and in-body `impl Trait { }`.** Both sit on the type's own declaration, so they are same-package by construction.
- **Cross-*package* impls with one end at home.** Implementing your own trait for a foreign type is fine; so is implementing a foreign trait for your own type. Only the third-party case is refused.

A **built-in** trait (`Display`, `Comparable`, …) and a trait provided by a native extension belong to no package, so the type's package is the only home an impl of one has: `impl Display for SomeoneElsesType {}` lives there.

### The escape hatch: a newtype

To give a foreign type behavior from your own package, wrap it in a type you own. `@derive(Trait, via: field)` is [the newtype pattern without the boilerplate](Derives#delegating-through-a-field-via), forwarding the whole trait through the field:

```noeta ignore
@derive(Speaks, via: inner)
class MyThing { pub inner: Thing }
```

The behavior is then yours, scoped to your type, and visible to exactly the code that asks for it. E0070's help prints this sketch with your own names filled in.

## Operator errors

Applying an operator to a type that does not implement its trait is an error. `+` or `<` on a plain struct is E0007. `===` on a value type, meaning a struct, is E0034, since identity is a class-only concept.
