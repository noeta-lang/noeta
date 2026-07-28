# Structs, Classes & Enums

The language has two aggregate kinds and one sum kind. `struct` and `class` share the *same body grammar* — they differ only in **semantics**. `enum` models a closed set of alternatives.

## The value/reference distinction

| | `struct` | `class` |
|---|---|---|
| Semantics | **value** | **reference** |
| Identity | none | yes (`===` / `!==`) |
| Default `==` | structural (field-wise) | identity |
| Mutating a `mut` field | copy-on-write rebind | in-place, visible to all aliases |
| Requires for field-set | `mut` binding **and** `mut` field | `mut` field only |
| `destruct` block | not allowed | allowed |

The choice is about whether a value *is* its contents (a `Point` is its `x` and `y`) or has an *identity* that persists across mutation (an `Order` you keep updating).

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
    fn new(v: int): Box { return Box { n: v } }
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

Fields are declared `name: T`, one per line (or `;`-separated). They are **private by default**:

- Reading a non-`pub` field from outside the type is E0035.
- Assigning a non-`mut` field is E0033.
- A private field is readable inside *any* method of the declaring type, on *any* value of that type (`other.x`), not just `self`.

```noeta
class Account {
    pub name: string      // readable outside
    mut balance: int      // assignable, but private (read only inside methods)
}
```

### Per-field defaults

`name: T = expr` makes a field **optional** in a literal, filled at construction. Defaults are evaluated in the type's **definition (global) scope** — they resolve globals only, never `self`, siblings, or the call site. A heap default (like a list) is rebuilt on each construction.

```noeta
struct Cfg {
    name: string
    retries: int = 3
    tags: List<int> = [1, 2]
}
cfg = Cfg { name: "svc" }   // retries = 3, tags = [1, 2]
```

## Constructing values

The all-fields literal `T { f: v, … }` must set every non-defaulted field (a missing one is E0009).

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

The **empty literal** `T {}` is valid iff every field has a default:

```noeta
struct Defaults {
    retries: int = 3
    verbose: bool = false
}
d = Defaults {}         // ok — every field of Defaults has a default
echo d.retries          // 3
```

**Spread** `T { ...base, f: override }` fills every field you don't list explicitly from `base`. It is **position-independent**: an explicitly listed field wins whether it is written before or after the `...base`. The original is unchanged (structural update):

```noeta
struct Money { amount: int  currency: string }

a = Money { amount: 100, currency: "USD" }
b = Money { amount: 300, ...a }    // amount: 300, currency: "USD"
c = Money { ...a, amount: 300 }    // the same — the explicit field wins either way
echo b == c        // true
echo a             // Money {amount: 100, currency: "USD"} — the original is unchanged
```

## Methods and `self`

Methods live in the type body. Member access is **explicit**: a field is read and written through `self.field` — a bare name inside a method is always a local (or an unknown name), never a field:

```noeta
class Counter {
    pub mut n: int
    fn new(): Counter { return Counter { n: 10 } }   // associated function (no self)
    fn read(): int { return self.n }                  // fields read through self
    fn set_then_read(): int {
        self.n = 5                                     // self.f = v writes the field
        return self.n                                  // 5
    }
}
```

> [!IMPORTANT]
> A bare name (`n`) never touches the receiver: `n = 5` declares a local, and reading `n` without a local in scope is a compile-time unknown-name error with a hint (`use self.n`). Index-assign through a field works too: `self.cells[i] = v` desugars to `self.cells = self.cells.set(i, v)`.

**Associated functions** take no receiver (constructors are the usual case) and are called on the bare type name; **methods** dispatch on a value:

```noeta check
c = Counter.new()   // associated function
c.set_then_read()   // method
```

**A field holding a function is callable through the receiver**: `obj.f(args)` means `(obj.f)(args)` when `f` is a field of function type (see [Functions & Closures](Functions-and-Closures#calling-a-closure-valued-field)). If a method and a field share a name, the method wins in call position and the field in value position — `g = obj.f; g(x)` always reaches the field.

## Destructors (class only)

A `class` may declare `destruct { … }`, which runs when the instance is dropped — at its **last use**, not at scope end (see [Memory Management](Memory-Management)). Locals drop in reverse declaration order. A container that owns a class (e.g. a generic `Box<T>` holding one) fires its destructor transitively.

```noeta
class File {
    path: string
    destruct { echo "closing ${self.path}" }
}
```

## Enums

An `enum` is a closed set of variants — plain, payload-carrying, or string-backed. Variants are `;`-separated.

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

A payload field may be **named** (`NegativePrice(index: int)`) or **positional** (`NegativePrice(int)`) — the name is documentation, since construction and `match` both bind by position. Either way the payload is a *type*, so it may be anything a type annotation may: an imported or fully-qualified name, generic arguments, `?T`, a tuple, a function type.

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

Construct with `Enum.Variant` (or `Enum.Variant(payload)`), compare with `==`, and destructure in a `match`:

```noeta
enum OrderError { Empty; NegativePrice(index: int) }

e = OrderError.NegativePrice(index: 2)
echo match e {
    OrderError.Empty            => "empty",
    OrderError.NegativePrice(i) => "item ${i}",
}
```

Enums share the unified body grammar — they can hold methods and `impl Trait { }` blocks. An **instance method's `self` is the whole enum value** (reach the payload by matching); associated functions are called on the type name:

```noeta
enum Level {
    Low; Mid; High
    fn rank(): int {
        return match self { Level.Low => 0, Level.Mid => 1, Level.High => 2 }
    }
}
echo Level.High.rank()   // 2
```

A **string-backed** enum gets `Enum.try_from(s): ?Enum` (name-matched, `none` on miss) and `Enum.from(s): Enum` (panics on miss). Payload variants are not name-constructible.

## Tuples

Tuples are anonymous, positional, value-semantic aggregates. A literal needs **2 or more** elements — `(x)` is just a parenthesized expression, and `()` is unit.

```noeta
fn divmod(a: int, b: int): (int, int) { return (a / b, a % b) }

p = (1, "two", 3.0)
echo p.0                        // 1  (access by position)
echo p == (1, "two", 3.0)       // true  (structural equality)

(q, r) = divmod(17, 5)          // destructuring binding

nested = ((1, 2), (3, 4))
echo nested.0.1                 // 2  (nested projection)
```

Their type is written positionally — `(int, string)` — and they are the idiom for [returning multiple values](Functions-and-Closures#multiple-return-via-tuples).

## See also

- [Generics & Traits](Generics-and-Traits) — generic types (`class Box<T>`) and `@derive`.
- [The Type System](Type-System) — the abstract kind-types `Struct`/`Class`/`Enum`.
- [Memory Management](Memory-Management) — how value semantics, COW, and destructors are implemented.
