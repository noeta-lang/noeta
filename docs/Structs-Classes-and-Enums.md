# Structs, Classes & Enums

The language has two aggregate kinds and one sum kind. `struct` and `class` share the *same body grammar* and differ in **semantics**. `enum` models a closed set of alternatives.

## The value/reference distinction

| | `struct` | `class` |
|---|---|---|
| Semantics | **value** | **reference** |
| Identity | none | yes (`===` / `!==`) |
| Default `==` | structural (field-wise) | identity |
| Mutating a `mut` field | copy-on-write rebind | in-place, visible to all aliases |
| Requires for field-set | `mut` binding **and** `mut` field | `mut` field only |
| `destruct` block | not allowed | allowed |

Choose by asking whether a value *is* its contents, the way a `Point` is its `x` and `y`, or has an *identity* that persists across mutation, the way an `Order` you keep updating does.

```noeta check
enum Status { Open; Shipped }

struct Point { x: int  y: int }        // value type

class Order {
    id: int
    mut status: Status
    // ...
}
```

Aliasing makes the difference concrete:

```noeta
class Box { pub mut n: int
    pub fn new(v: int): Box { return Box { n: v } }
}
struct Counter { mut n: int }

// class: an alias sees in-place mutation
mut b = Box.new(10)
alias = b
b.n = 20
echo alias.n         // 20

// struct: a snapshot keeps the old value (copy-on-write)
mut c = Counter { n: 1 }
snap = c
c.n = 9
echo snap.n          // 1
```

## Fields

Fields are declared `name: T`, one per line or `;`-separated. A **`class`**'s fields are **private by default**; a **`struct`**'s and an **`enum`** payload's are always public.

- Reading or setting a non-`pub` field of a `class` from outside the type is E0035. A `struct` never raises it, its fields being public, and writing `pub` on one is refused (E0077).
- Assigning a non-`mut` field is E0033, in every kind.
- A private field is readable inside *any* method of the declaring type, on *any* value of that type (`other.x`), not just `self`.

```noeta
class Account {
    pub name: string      // readable outside
    mut balance: int      // assignable, but private (read only inside methods)
}
```

A struct's fields are public because a value **is** its contents. Structural `==` already compares them field by field, and copy-on-write means there is no shared instance whose invariant could be broken behind your back. A class has an identity that outlives any one assignment, so it can keep an invariant, and default-private is what lets it.

Whether the *operations* are public is a separate question, answered the same way for every kind. See [Method visibility](#method-visibility) below.

### Per-field defaults

`name: T = expr` makes a field **optional** in a literal, filled at construction. Defaults are evaluated in the type's **definition (global) scope**, so they resolve globals only, never `self`, siblings, or the call site. A heap default such as a list is rebuilt on each construction.

```noeta
struct Cfg {
    name: string
    retries: int = 3
    tags: List<int> = [1, 2]
}
cfg = Cfg { name: "svc" }   // retries = 3, tags = [1, 2]
```

## Constructing values

The all-fields literal `T { f: v, … }` must set every non-defaulted field, and every initializer must name a field the type declares. A missing field is E0009 and an undeclared name is E0005, both reported by `noeta check` wherever the literal names its type.

```noeta check
p = Point { x: 1, y: 2 }
```

**Field-init shorthand** puns an in-scope variable of the same name:

```noeta
struct User { name: string  email: string }

name = "Ada"; email = "ada@x.io"
u = User { name, email }    // ≡ User { name: name, email: email }
echo u                      // User {name: "Ada", email: "ada@x.io"}
```

The **empty literal** `T {}` is valid exactly when every field has a default:

```noeta
struct Defaults {
    retries: int = 3
    verbose: bool = false
}
d = Defaults {}         // ok — every field of Defaults has a default
echo d.retries          // 3
```

**Spread** `T { ...base, f: override }` fills every field you do not list explicitly from `base`. It is **position-independent**, so an explicitly listed field wins whether it is written before or after the `...base`. The original is unchanged, this being a structural update:

```noeta
struct Money { amount: int  currency: string }

a = Money { amount: 100, currency: "USD" }
b = Money { amount: 300, ...a }    // amount: 300, currency: "USD"
c = Money { ...a, amount: 300 }    // the same — the explicit field wins either way
echo b == c        // true
echo a             // Money {amount: 100, currency: "USD"} — the original is unchanged
```

Spreading a `dyn` value fills whichever fields that value turns out to carry, and that is settled when the object is built. A literal whose remaining fields come only from such a spread raises E0009 at construction. A spread fills declared fields only, so an undeclared name written beside it is still E0005 at `noeta check`.

## Methods and `self`

Methods live in the type body. Member access is **explicit**: a field is read and written through `self.field`, and a bare name inside a method is always a local or an unknown name.

```noeta
class Counter {
    pub mut n: int
    pub fn new(): Counter { return Counter { n: 10 } }   // static function (no self)
    pub fn read(): int { return self.n }              // fields read through self
    pub fn set_then_read(): int {
        self.n = 5                                     // self.f = v writes the field
        return self.n                                  // 5
    }
}
```

> [!IMPORTANT]
> A bare name (`n`) never touches the receiver. `n = 5` declares a local, and reading `n` without a local in scope is a compile-time unknown-name error with a hint (`use self.n`). Index-assign through a field works too: `self.cells[i] = v` desugars to `self.cells = self.cells.set(i, v)`.

**Associated functions** take no receiver, constructors being the usual case, and are called on the bare type name. **Methods** dispatch on a value:

```noeta check
c = Counter.new()   // static function
c.set_then_read()   // method
```

**A field holding a function is callable through the receiver.** `obj.f(args)` means `(obj.f)(args)` when `f` is a field of function type (see [Functions & Closures](Functions-and-Closures#calling-a-closure-valued-field)). If a method and a field share a name, the method wins in call position and the field in value position, so `g = obj.f; g(x)` always reaches the field.

## One name, one member

Within a declaration, a name belongs to one member of its kind. Two fields called `x`, two variants called `A`, or two methods called `f` in one body is **E0080**, in every kind — `struct`, `class`, `enum` and `trait` alike:

```noeta error
struct Point {
    x: int
    y: int
    x: int      // E0080: `Point` declares field `x` more than once
}
```

Nothing picks a winner, because there is no answer that keeps both: whichever member is dropped, the code written for it stops running with nothing to say so. Rename one, or delete the one you did not mean to keep.

Fields and methods are separate name spaces, so the `f`-field-and-`f`-method pair above is not a duplicate — it is the resolution rule described just above, and it stands.

The rule earns its keep where one of the two members was **generated**. A [directive that generates code](Native-Extensions#directives-that-generate-code) writes its members from a document outside the program — an interface description, a schema — and adds them to the declaration it decorates. So a name can be taken by something nobody typed, on a day nobody edited this file:

```noeta ignore
@openapi("petstore.json")
pub struct PetStore {
    // E0080 if the spec's servers already give `PetStore` a `base_url`.
    pub fn base_url(): string { return "https://staging.example/api/v3" }
}
```

The diagnostic names both halves: the generated member, in the expansion source `noeta expand` prints, and the hand-written one it collides with. Whichever you keep, say so explicitly — rename the method you wrote, or take the name out of the document the generator reads.

## `Self` — the type in hand

Inside a type body, `Self` names the type it is the body of. Write it anywhere a type goes: a return, a parameter, a field, an enum payload, and at any depth (`List<Self>`, `?Self`).

```noeta
class Counter {
    pub mut n: int
    pub fn new(): Self { return Counter { n: 0 } }
    pub fn bump(): Self { self.n = self.n + 1; return self }
}

class Link {
    pub mut next: ?Self          // a self-referential field
    pub fn new(): Self { return Link { next: none } }
}

enum Tree {
    Leaf(int);
    Node(Self, Self);            // a recursive payload
}
```

The capital is the whole difference between the two spellings. `self` is the **value** the method was called on, and `Self` is its **type**, which is why `self.n` reads a field and `fn bump(): Self` declares what comes back.

Inside a generic type, `Self` is that type at its own instantiation. `Self` in `class Box<T>` is `Box<T>`, so a method returning `Self` keeps the element type its receiver had.

Reflection and narrowing resolve it to the same type everything else does: `type_name::<Self>()` answers with the declaring type's name, and `v is Self` matches its values.

Nothing may be *declared* `Self`. No struct, class, enum or trait may take the name, because inside any type body the spelling already means the enclosing type. In a trait declaration `Self` means the implementing type; see [Implementing a trait](Generics-and-Traits#implementing-a-trait).

## Method visibility

A method is **private by default**, and `pub` puts it on the type's surface. This is one rule for `class`, `struct` and `enum` alike, where fields take their default per kind.

```noeta
class Account {
    mut balance: int = 0
    fn fee(): int { return self.balance / 100 }          // internal
    pub fn charge(): int { return self.balance - self.fee() }
}

a = Account { }
echo a.charge()   // fine
// a.fee()        // E0076: cannot call private method `fee` of `Account` from outside it
```

- Calling a non-`pub` method from outside its type is E0076, whether by `x.m(…)`, `T.m(…)`, either turbofish form, `obj(…)` through the `Callable` protocol, or by binding a handle (`f = T.m`).
- A private method is reachable inside *any* method of the declaring type, on *any* value of that type (`other.m()`), exactly as a private field is.
- A `@test`/`@doc` body sees its module's privates, dev tiers being white-box by design.

Fields and methods answer different questions, which is why they do not share a default. A struct's fields are public because a value *is* its contents, and that says nothing about which operations belong to the type's API. A `Point` whose `x` and `y` are visible still benefits from keeping a helper internal.

A method that implements a `trait` must be written `pub`:

```noeta
trait Speaks { fn speak(): string }

class Dog {
    impl Speaks {
        pub fn speak(): string { return "woof" }
    }
}
```

A trait is an outward contract, since anyone holding a `dyn Speaks` calls the method, so the implementation is on the public surface by construction. Omitting `pub` is E0015. Writing it keeps an `impl` block readable on its own, without first knowing which names the trait declares.

Inside a `trait`'s *own* declaration `pub` is refused (E0053). Every method a trait declares is already its contract, and writing the word there would suggest the unmarked ones are private.

## Destructors (class only)

A `class` may declare `destruct { … }`, which runs when the instance is dropped, at its **last use** rather than at scope end (see [Memory Management](Memory-Management)). Locals drop in reverse declaration order. A container that owns a class, such as a generic `Box<T>` holding one, fires its destructor transitively.

```noeta
class File {
    path: string
    destruct { echo "closing ${self.path}" }
}
```

## Enums

An `enum` is a closed set of variants: plain, payload-carrying, or string-backed. Variants are `;`-separated.

```noeta
enum Status { Pending; Paid; Refunded }                 // plain

enum OrderError {
    Empty
    NegativePrice(index: int)                            // algebraic payload
}

enum Direction: string {                                 // string-backed
    North = "N"
    South = "S"
}
```

A payload field may be **named** (`NegativePrice(index: int)`) or **positional** (`NegativePrice(int)`). The name is documentation, since construction and `match` both bind by position. Either way the payload is a *type*, so it may be anything a type annotation may: an imported or fully-qualified name, generic arguments, `?T`, a tuple, a function type.

```noeta
use std.id.Uuid

enum Event {
    Started(Uuid)                                        // an imported type
    Tagged(List<string>)                                 // generic arguments
    Scored(name: string, points: int)                    // named, several fields
    Missing
}
echo match Event.Tagged(["a", "b"]) {
    Event.Started(u)   => "start",
    Event.Tagged(tags) => "${tags.len()} tags",
    Event.Scored(n, p) => "${n}=${p}",
    Event.Missing      => "-",
}
```

Construct with `Enum.Variant`, or `Enum.Variant(payload)`, compare with `==`, and destructure in a `match`:

```noeta
enum OrderError { Empty; NegativePrice(index: int) }

e = OrderError.NegativePrice(index: 2)
echo match e {
    OrderError.Empty            => "empty",
    OrderError.NegativePrice(i) => "item ${i}",
}
```

Enums share the unified body grammar, so they can hold methods and `impl Trait { }` blocks. An **instance method's `self` is the whole enum value**, and the payload is reached by matching. Static functions are called on the type name:

```noeta
enum Level {
    Low; Mid; High
    pub fn rank(): int {
        return match self { Level.Low => 0, Level.Mid => 1, Level.High => 2 }
    }
}
echo Level.High.rank()   // 2
```

### Converting a wire value to a case

Every enum gets a pair: `Enum.try_from(v): ?Enum` gives `none` on a miss, and `Enum.from(v): Enum` panics on one. That is the recoverable and aborting shape the [rest of the language uses](Error-Handling#aborting-and-recoverable-doors). Reach for `try_from` whenever the value came from outside the program, and for `from` when a bad value means the program itself is wrong.

The value is matched against each case's **backing first, then its name**. A backed enum's backing is what its JSON Schema advertises and what a real document carries, so that is what a wire-facing conversion reads:

```noeta
enum Plan: string { Free = "free"; Paid = "paid" }
enum Code: int { Ok = 200; Missing = 404 }

echo Plan.try_from("free")     // some(Plan.Free)   — the backing
echo Plan.try_from("Free")     // some(Plan.Free)   — the case name still works
echo Plan.try_from("gold")     // none
echo Code.try_from(404)        // some(Code.Missing) — backings are typed, so this takes an int
echo Plan.from("paid")         // Plan.Paid
```

A **plain** enum has no backings, so its case names are what select, which is also what its schema advertises. The argument type follows the backing: a `string`-backed or plain enum takes a `string`, and an `int`-backed one takes `int | string`.

Payload-carrying variants are never selected, there being no payload to supply. Build those with [`construct("Enum.Variant", payload)`](Attributes-and-Reflection#constructing-an-enum-case).

This pair stays available on an enum that also declares an [`impl From<Source>`](Error-Handling#converting-errors-at---impl-fromsource). Reading a wire value and running a declared conversion are different operations, and a declared conversion carries its source in its name, so `Plan.from("free")` and `Plan.from(raw)` both mean what they say.

To decode an enum sitting inside a larger document, derive [`Deserialize<Json>`](Derives#enum-typed-fields) on the enclosing type instead. The same backing-versus-name rule applies there, with path-carrying errors.

## Tuples

Tuples are anonymous, positional, value-semantic aggregates. A literal needs **2 or more** elements, since `(x)` is just a parenthesized expression. There is no one- or zero-element form: `()` is a parse error (E0003) in every position, binding, argument, return value and annotation alike. The unit *type* is spelled `void`, and a `void` function returns with a bare `return;` or by falling off the end of its body.

```noeta
fn divmod(a: int, b: int): (int, int) { return (a / b, a % b) }

p = (1, "two", 3.0)
echo p.0                        // 1  (access by position)
echo p == (1, "two", 3.0)       // true  (structural equality)

(q, r) = divmod(17, 5)          // destructuring binding

nested = ((1, 2), (3, 4))
echo nested.0.1                 // 2  (nested projection)
```

Their type is written positionally, `(int, string)`, and they are the idiom for [returning multiple values](Functions-and-Closures#multiple-return-via-tuples).

## See also

- [Generics & Traits](Generics-and-Traits) — generic types (`class Box<T>`) and `@derive`.
- [The Type System](Type-System) — the abstract kind-types `Struct`/`Class`/`Enum`.
- [Memory Management](Memory-Management) — how value semantics, COW, and destructors are implemented.
