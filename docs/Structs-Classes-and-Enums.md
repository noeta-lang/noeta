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

```lang ignore
struct Point { x: int  y: int }        // value type

class Order {
    id: int
    mut status: Status
    // ...
}
```

Aliasing makes the difference concrete:

```lang
class Box { pub mut n: int
    fn new(v: int): Box { return Box { n: v } }
}
struct Cell { mut n: int }

// class: an alias sees in-place mutation
mut b = Box.new(10)
alias = b
b.n = 20
echo alias.n         // 20

// struct: a snapshot keeps the old value (copy-on-write)
mut c = Cell { n: 1 }
snap = c
c.n = 9
echo snap.n          // 1
```

## Fields

Fields are declared `name: T`, one per line (or `;`-separated). They are **private by default**:

- Reading a non-`pub` field from outside the type is E0035.
- Assigning a non-`mut` field is E0033.
- A private field is readable inside *any* method of the declaring type, on *any* value of that type (`other.x`), not just `self`.

```lang
class Account {
    pub name: string      // readable outside
    mut balance: int      // assignable, but private (read only inside methods)
}
```

### Per-field defaults

`name: T = expr` makes a field **optional** in a literal, filled at construction. Defaults are evaluated in the type's **definition (global) scope** — they resolve globals only, never `self`, siblings, or the call site. A heap default (like a list) is rebuilt on each construction.

```lang
struct Cfg {
    name: string
    retries: int = 3
    tags: List<int> = [1, 2]
}
cfg = Cfg { name: "svc" }   // retries = 3, tags = [1, 2]
```

## Constructing values

The all-fields literal `T { f: v, … }` must set every non-defaulted field (a missing one is E0009).

```lang ignore
p = Point { x: 1, y: 2 }
```

**Field-init shorthand** puns an in-scope variable of the same name:

```lang ignore
name = "Ada"; email = "ada@x.io"
u = User { name, email }    // ≡ User { name: name, email: email }
```

The **empty literal** `T {}` is valid iff every field has a default:

```lang ignore
c = Cfg { name: "x" }   // ok
d = Defaults {}         // ok only if every field of Defaults has a default
```

**Spread** `T { ...base, f: override }` copies every field from `base`, then applies overrides. The original is unchanged (structural update):

```lang ignore
a = Money { amount: 100, currency: "USD" }
b = Money { amount: 300, ...a }    // amount: 300, currency: "USD"
```

## Methods and `self`

Methods live in the type body. Inside a method, a **bare field name reads the field** off the receiver — no `self.` needed:

```lang
class Counter {
    pub mut n: int
    fn new(): Counter { return Counter { n: 10 } }   // associated function (no self)
    fn read(): int { return n }                       // bare `n` reads the field
    fn set_then_read(): int {
        self.n = 5                                     // self.f = v writes the field
        return n                                       // 5
    }
}
```

> [!IMPORTANT]
> A bare **read** (`n`) loads the field, but a bare **write** (`n = 5`) creates a *local* that shadows the field — it does not touch the receiver. To mutate a field, always use the explicit `self.f = v`. Index-assign through a field works too: `self.cells[i] = v` desugars to `self.cells = self.cells.set(i, v)`.

**Associated functions** take no receiver (constructors are the usual case) and are called on the bare type name; **methods** dispatch on a value:

```lang ignore
c = Counter.new()   // associated function
c.set_then_read()   // method
```

## Destructors (class only)

A `class` may declare `destruct { … }`, which runs when the instance is dropped — at its **last use**, not at scope end (see [Memory Management](Memory-Management)). Locals drop in reverse declaration order. A container that owns a class (e.g. a generic `Box<T>` holding one) fires its destructor transitively.

```lang
class File {
    path: string
    destruct { echo "closing ${path}" }
}
```

## Enums

An `enum` is a closed set of variants — plain, payload-carrying, or string-backed. Variants are `;`-separated.

```lang
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

Construct with `Enum.Variant` (or `Enum.Variant(payload)`), compare with `==`, and destructure in a `match`:

```lang
enum OrderError { Empty; NegativePrice(index: int) }

e = OrderError.NegativePrice(index: 2)
echo match e {
    OrderError.Empty            => "empty",
    OrderError.NegativePrice(i) => "item ${i}",
}
```

Enums share the unified body grammar — they can hold methods and `impl Trait { }` blocks. An **instance method's `self` is the whole enum value** (reach the payload by matching); associated functions are called on the type name:

```lang
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

```lang
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
