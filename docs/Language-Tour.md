# Language Tour

A guided, example-driven walk through the whole language in one sitting. Every snippet runs with `noeta run`. If you have not installed the toolchain yet, start with [Getting Started](Getting-Started).

This tour teaches by building up. For the exhaustive rules on any topic, follow the links to the reference pages.

---

## Values and bindings

A binding names a value. Bindings are immutable by default; add `mut` to reassign.

```noeta
name = "Ada"        // immutable
mut count = 0       // mutable
count = count + 1
count += 1          // compound assignment: count = count + 1
echo count          // 2
```

Reassigning an immutable binding is a compile error. A `mut` binding also keeps a **fixed type** — a reassignment must match it (`count = "two"` is an error); declare a union or `dyn` for a multi-type binding. You can annotate a binding's type — it is checked, then erased at runtime:

```noeta
xs: List<int> = [1, 2, 3]
```

The primitive types are `int` (64-bit, wraps on overflow), `float`, `f32`, `f64`, `bool`, `string`, and `void`. Number literals support underscores and radix prefixes:

```noeta
echo 1_000_000     // 1000000
echo 0xFF          // 255
echo 0b1010        // 10
echo 1.5e3         // 1500.0
```

→ Full details: [Syntax Basics](Syntax-Basics).

---

## Strings

There are three string forms:

```noeta
name = "Niro"
echo "Hello ${name}"          // "..."  interpolated: ${expr} embeds any expression
echo 'literal ${name}'        // '...'  raw: no interpolation, no escapes but \' and \\
echo `
    Dear ${name},
    Welcome aboard.
`                             // `...`  dedented multiline template (great for SQL/HTML)
```

The `~` operator concatenates (display-concatenating non-strings):

```noeta
echo "users/" ~ 42 ~ "/profile"    // users/42/profile
```

Strings carry a rich method set — `.upper()`, `.trim()`, `.split(",")`, `.replace(a, b)`, `.contains(s)`, and more. See [Built-ins](Standard-Library).

---

## Control flow

`if`/`else if`/`else` with mandatory braces:

```noeta
n = 1
if n == 0 {
    echo "zero"
} else if n == 1 {
    echo "one"
} else {
    echo "many"
}
```

`if … then … else` is also an **expression** (note the `then` keyword):

```noeta
n = 50
label = if n > 10 then "big" else "small"
echo label                    // big
```

`while` and `for … in`:

```noeta
mut i = 0
while i < 3 { echo i; i += 1 }

for n in [1, 2, 3] { echo n }
for k in 0..5 { echo k }                    // 0 1 2 3 4  (exclusive range)
for (idx, x) in ["a", "b"].enumerate() {    // destructure the (index, value) tuple
    echo "${idx}:${x}"
}
```

(No shadowing: the loop variables pick names distinct from the `mut i` above — one name, one meaning, per scope.)

`break` and `continue` work as expected. → [Control Flow & Pattern Matching](Control-Flow-and-Pattern-Matching).

---

## Functions

Named functions require types on their parameters and return value. Bodies infer everything else.

```noeta
fn add(a: int, b: int): int {
    return a + b
}

fn fib(n: int): int {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}

echo add(2, 3)     // 5
echo fib(10)       // 55
```

Trailing parameters can have **defaults**:

```noeta
fn greet(name: string, greeting: string = "Hello"): string {
    return "${greeting}, ${name}!"
}
echo greet("Ada")           // Hello, Ada!
echo greet("Ada", "Hi")     // Hi, Ada!
```

An argument can **name** the parameter it fills, in any order — which also lets a call skip a defaulted parameter and supply a later one:

```noeta
fn f(a: int, b: int = 2, c: int = 3): int { return a * 100 + b * 10 + c }

echo f(b: 5, a: 1)   // 153
echo f(1, c: 9)      // 129 — `b` still defaults
```

See [Functions and Closures](Functions-and-Closures#named-arguments).

Return several values with a **tuple**:

```noeta
fn divmod(a: int, b: int): (int, int) {
    return (a / b, a % b)
}
(q, r) = divmod(17, 5)      // q = 3, r = 2
```

### Closures and the pipe

Closures are `fn(x) => expr` (arrow) or `fn(x) { … }` (block). Their types are inferred:

```noeta
inc = fn(x) => x + 1
twice = fn(f, x) => f(f(x))
echo twice(inc, 10)         // 12
```

The pipe `|>` threads a value into the next call as its first argument — or, if a [label](Functions-and-Closures#piping-into-a-parameter-that-isnt-the-first) has already claimed that parameter, into the first one still free. Either way it reads left-to-right:

```noeta
fn double(n: int): int { return n * 2 }

echo 5 |> double |> double          // 20

// Collection work chains directly as methods:
echo [1, 2, 3, 4]
    .filter(fn(n) => n % 2 == 0)
    .map(fn(n) => n * 10)
    .sum()                          // 60
```

→ [Functions & Closures](Functions-and-Closures).

---

## Modeling data: structs, classes, enums

Two aggregate kinds, distinguished by **semantics**:

- **`struct`** — a *value*. No identity; compares field-by-field; assigning a field copies-on-write.
- **`class`** — a *reference*. Has identity (`===`); shared by reference; a `mut` field mutates in place, visible to every alias.

```noeta
struct Point { x: int  y: int }        // value type

class Counter {
    pub mut n: int
    pub fn new(): Self { return Counter { n: 0 } }      // static function (no self)
    pub fn bump(): void { self.n = self.n + 1 }         // method; fields read through self
}

p = Point { x: 1, y: 2 }
echo p == Point { x: 1, y: 2 }         // true  (structural equality)

c = Counter.new()
c.bump()
echo c.n                                // 1
```

A `class`'s fields are private by default; mark them `pub` to read from outside, `mut` to allow assignment. (A `struct`'s fields are always public — a value *is* its contents — so writing `pub` on one is refused, E0077.) Fields can have defaults, and literals support shorthand (`Point { x, y }`) and spread (`Point { ...p, x: 9 }`).

**Methods are private by default in every kind**, and `pub` puts one on the type's surface — so a `struct` with public data can still keep a helper to itself. A method implementing a `trait` must be written `pub`, because a trait is an outward contract. See [Method visibility](Structs-Classes-and-Enums#method-visibility).

**Enums** model a closed set of alternatives, optionally carrying data:

```noeta
enum Status { Pending; Paid; Refunded }

enum OrderError {
    Empty
    NegativePrice(index: int)          // a variant with a payload
}

enum Direction: string {               // string-backed
    North = "N"
    South = "S"
}
```

All three kinds share the same body grammar — they can hold methods and `impl Trait { }` blocks.

**`Self` names the type in hand.** Inside any type body it stands for the type it is the body of, so a declaration never repeats its own name — `fn new(): Self` above, a self-referential field (`next: ?Self`), a recursive payload (`Node(Self, Self)`). It is `self`'s type: lowercase is the value, capitalized is what that value is.

→ [Structs, Classes & Enums](Structs-Classes-and-Enums).

---

## Pattern matching

`match` is an expression, and it is checked for exhaustiveness — a missing case (with no `_`) is a compile error:

```noeta check
enum Status { Pending; Paid; Refunded }

fn label(s: Status): string {
    return match s {
        Status.Pending  => "awaiting payment",
        Status.Paid     => "paid",
        Status.Refunded => "refunded",
    }
}
```

Patterns bind payloads, destructure tuples, and match literals:

```noeta
fn classify(p: (int, int)): string {
    return match p {
        (0, 0) => "origin",
        (x, 0) => "on x-axis at ${x}",
        (0, y) => "on y-axis at ${y}",
        (x, y) => "at ${x},${y}",
    }
}
```

→ [Control Flow & Pattern Matching](Control-Flow-and-Pattern-Matching).

---

## Collections

Lists, maps, and sets — all value-semantic (copy-on-write):

```noeta
xs = [1, 2, 3]
echo xs[0]                     // 1
echo xs.len()                  // 3
echo xs.reverse()              // [3, 2, 1]
echo [...xs, 4]                // [1, 2, 3, 4]  (spread)

m = {"a": 1, "b": 2}
echo m["a"]                    // 1
echo m.keys()                  // ["a", "b"]  (sorted)

s = #{3, 1, 2, 1}              // set literal: sorted + de-duplicated
echo s                         // {1, 2, 3}
echo s.contains(2)             // true
```

Lazy **iterators** let you compose transformations without building intermediate lists:

```noeta
echo [1, 2, 3, 4, 5].iter()
    .map(fn(n) => n * 10)
    .take(3)
    .collect()                 // [10, 20, 30]
```

→ [Built-ins](Standard-Library).

---

## Errors: no `null`, no exceptions

Absence and failure are ordinary values.

- **`Option`** — `?T`. Constructed with `some(x)` / `none`.
- **`Result<T, E>`** — constructed with `Ok(x)` / `Err(e)`.

```noeta
fn pick(hit: bool): ?int {
    if hit { return some(7) }
    return none
}

echo pick(true) ?? 0           // 7    (?? supplies a fallback for none)
echo pick(false) ?? 0          // 0
```

The `?` operator propagates a failure, early-returning it from the current function:

```noeta
struct Item { price: float  qty: int }

enum OrderError { Empty }

class Order {
    pub items: List<Item>
    pub fn new(items: List<Item>): Order { return Order { items: items } }
}

// sample:start
fn validate(items: List<Item>): Result<void, OrderError> {
    if items.len() == 0 { return Err(OrderError.Empty) }
    return Ok()
}

fn place(items: List<Item>): Result<Order, OrderError> {
    validate(items)?                        // returns the Err here if invalid
    return Ok(Order.new(items))
}

echo match place([Item { price: 9.99, qty: 2 }]) {
    Ok(o)  => "placed ${o.items.len()} item(s)",
    Err(_) => "rejected",
}                                           // placed 1 item(s)

echo match place([]) {
    Ok(_)  => "placed",
    Err(_) => "rejected",
}                                           // rejected
```

→ [Error Handling](Error-Handling).

---

## Generics and traits

Generics let one definition serve many types without giving up checking — a `Box<T>` holds any `T`, and `max<T: Comparable>` accepts any type that can be ordered. Type parameters can be bounded by a built-in trait:

```noeta
class Box<T> {
    pub value: T
    fn new(v: T): Box<T> { return Box { value: v } }
}

fn max<T: Comparable>(a: T, b: T): T {
    if a > b { return a }
    return b
}
```

Operators dispatch through a fixed set of built-in traits (`Equatable` → `==`, `Comparable` → `<`, `Display` → `echo`, `Add` → `+`, …). Implement them in a type's body, or synthesize them with `@derive`:

```noeta
@derive(Equatable, Comparable, Display)
class Money {
    amount: int
    pub fn new(a: int): Money { return Money { amount: a } }
}

echo Money.new(5) < Money.new(9)     // true
```

(Under the hood, one compiled shape serves every instantiation — type parameters do not affect dispatch — but values carry a reflected type tag, so `type_of` and `x is List<int>` still recover the type arguments.)

→ [Generics & Traits](Generics-and-Traits).

---

## Modules

Split code across files. A file **is** a module, and its path is derived from where the file sits — the package's import prefix plus the file's path inside the package — so there is nothing to declare. Declarations are private unless marked `pub`:

```noeta
// src/models.noe, in the package `local/shop`  →  the module `shop.models`

pub class User {
    pub name: string
    fn new(name: string): User { return User { name: name } }
}
```

```noeta check
// src/main.noe  →  the module `shop.main`

use shop.models.User

echo User.new("Ada").name        // Ada
```

The standard library is imported the same way:

```noeta
use std.{math, json}

echo math.sqrt(16.0)             // 4.0
echo json.stringify([1, 2, 3])   // [1,2,3]
```

→ [Modules & Visibility](Modules), the [standard library reference](Std).

---

## Concurrency

`async fn`, `.await`, and a structured `concurrent { }` scope; `spawn` for concurrent tasks, `isolate` for true-parallel ones, and typed channels for message passing:

```noeta
use std.task.{sleep, all}
async fn work(name: string, ms: int): int {
    echo "${name} start"
    sleep(ms).await
    return ms
}

concurrent {
    hs = [spawn work("a", 2), spawn work("b", 1)]
    xs = all(hs)                 // awaits all, results in input order
    echo "done: " ~ xs.join(",")
}
```

→ [Concurrency](Concurrency).

---

## A capstone

Putting it together — a small order pipeline (this is `examples/orders.noe`, trimmed):

```noeta
struct Item { price: float  qty: int }

enum OrderError {
    Empty
    NegativePrice(index: int)
}

fn total(items: List<Item>): float {
    return items.map(fn(it) => it.price * it.qty).sum()
}

fn validate(items: List<Item>): Result<void, OrderError> {
    if items.len() == 0 { return Err(OrderError.Empty) }
    for (i, item) in items.enumerate() {
        if item.price < 0 { return Err(OrderError.NegativePrice(index: i)) }
    }
    return Ok()
}

items = [Item { price: 9.99, qty: 2 }, Item { price: 4.50, qty: 1 }]

echo match validate(items) {
    Ok()   => "total: ${total(items)}",
    Err(e) => match e {
        OrderError.Empty            => "empty order",
        OrderError.NegativePrice(i) => "item ${i} has a negative price",
    },
}
```

```console
$ noeta run orders.noe
total: 24.48
```

## Next

You have seen the whole surface. For depth:

- **Conventions** — [how the ecosystem names things](Conventions), and which of those names the compiler enforces.
- **Reference** — the [Language reference](Home#language-reference) has one page per topic.
- **Tools** — [Testing](Testing), [Benchmarking](Benchmarking), [Dev Tiers](Dev-Tiers), and [Documentation](Documentation-and-Tiers) cover the `@test`/`@bench`/`@doc` workflow.
- **Under the hood** — [The Virtual Machine](The-Virtual-Machine) and [Memory Management](Memory-Management) explain how it all runs.
