# Language Syntax

*Working title: the language is referred to here as **the language**. Name TBD.*

This document specifies the surface syntax. The guiding philosophy is **coherent surface, powerful spine**: the surface is designed for clarity and consistency (it happens to read as broadly PHP-like, but that resemblance is incidental, not a target), while a small, opt-in set of powerful features (`Result`/`?`, ADTs, exhaustive `match`, pipelines, immutability-by-default) gives the language its reason to exist.

A core design discipline runs through everything: **every powerful feature has an obvious, readable surface and a simpler fallback**, so the language scales down to easy code as readily as up to rigorous code. Redundancy that aids legibility (keeping `return`, braces, and semicolons even where technically optional) is retained on purpose — it keeps the powerful constructs readable.

---

## 1. Departures from PHP at a glance

| PHP | This language | Why |
|---|---|---|
| `$name` | `name` | The `$` sigil is a historical lexer artifact; a real parser doesn't need it. |
| `$obj->prop` | `obj.prop` | `->` existed only because `.` was taken by concatenation. |
| `Foo::bar()` / `$obj->m()` | `Foo.bar()` / `obj.m()` | Static-vs-instance disambiguated by whether the left side is a type or a value; no separate sigil needed. |
| `"a" . "b"` (concat) | `"a" ~ "b"` | Frees `.` for member access; `~` is unambiguous and PHP devs know it from Twig. |
| `App\Models\User` | `App.Models.User` | `\` collides with string escapes and is unique to PHP. |
| `function foo()` | `fn foo()` | Terse, high-frequency; PHP already uses `fn` for arrows. |
| `int $x` (type before name) | `x: int` (name first) | Consistent with variable declarations; easier inference and optional typing. |
| `['a' => 1]` (dual-purpose array) | `[1, 2]` list / `{"a": 1}` map | Distinct types end the list/map conflation. `:` for map keys frees `=>`. |
| `=>` (keys, arrows, match arms) | `:` for map keys; `=>` only for arrows/match | One symbol, one job. |
| `readonly` everywhere | immutable by default; `mut` to opt in | The keyword you write is the *rare* case, not the common one. |
| `"Hello $name"` / `"{$expr}"` | `"Hello ${name}"` | One interpolation form, `${expr}`; a bare `{`, `}`, or `$` is literal (so JSON/regex need no escaping). The `$name` shorthand is deliberately omitted. |
| `if (): ... endif;` | braces only | The templating-era alternative syntax is dropped. |

Braces, semicolons, and common keyword names (`class`, `enum`, `match`, `return`, `for`, `if`, `echo`) are **kept** — not for PHP familiarity per se, but because they are clear, unambiguous, and keep the powerful constructs legible.

---

## 2. Variables and mutability

**Immutable by default. `mut` opts into mutation.** One uniform rule for both bindings and fields: *things do not change unless marked `mut`.*

```
name = "Niro";          // immutable binding
mut total = 0;          // mutable binding

total = total + 1;      // ok
// name = "other";      // compile error: `name` is immutable; add `mut` to allow reassignment
```

The error message for assigning to a non-`mut` binding is a first-class concern: it must name the binding, explain immutability, and suggest the `mut` fix inline.

---

## 3. Strings and interpolation

```
greeting = "Hello ${name}";             // ${expr} interpolation (bare braces are literal)
path = "users/" ~ id ~ "/profile";      // `~` concatenation
raw = 'a {json} blob, a $price, a \d+ regex';  // single quotes: no interpolation at all
template = `                              // backtick: dedented multiline template
    Dear ${name},
    Your order shipped.
`;                                        // → "Dear ${name},\nYour order shipped." (indent stripped)
```

Three string forms, by what they do with interpolation and whitespace:

| Form | Interpolation | Whitespace |
|---|---|---|
| `"..."` (double) | `${expr}` | literal (multiline allowed) |
| `'...'` (single) | none — raw | literal (multiline allowed) |
| `` `...` `` (backtick) | `${expr}` | **dedented** text block |

**Double-quoted** strings interpolate, triggering only on `${ expr }`; a bare `{`, `}`, or `$` is a literal character, so JSON and currency strings need no escaping. The one escape is `\${` for a literal dollar-brace. (There is no `$name` shorthand — `${name}` is always written in full, which keeps a stray `$` in prose harmless.)

**Single-quoted** strings are *raw*: no interpolation, and the only escapes are `\'` and `\\`. Everything else — `${...}`, braces, `$`, `\d`, `\n` — is verbatim, ideal for regex, Windows paths, and JSON blobs.

**Backtick** strings are *dedented templates*: `${expr}` interpolation like a double-quoted string, but the common leading indentation and a leading/trailing blank line are stripped (Kotlin `trimIndent` rules), so a multiline literal can be indented to match the surrounding code without that indentation leaking into the value — ideal for SQL, HTML, and email bodies.

---

## 4. Functions

Name-first parameter types, optional and inferable. `fn` keyword.

```
fn total(items: List<Item>): float {
    return items
        |> map(fn(it) => it.price * it.qty)
        |> sum();
}

// arrow form for short closures
adder = fn(a, b) => a + b;
```

**Optional parameters with default values.** A trailing parameter may carry a default (`= expr`); a call can then omit it. Defaults are **trailing-only** — a required parameter after a defaulted one is a compile error (`E0026`) — and a default's type must match its parameter. A default is evaluated in the function's *definition* scope (not where it is called) and does not see the function's own parameters: for a top-level function or method that is the module scope; for a closure it is the captured scope, so a closure default may reference a captured variable, like the closure body. Defaults are re-evaluated on each call that omits the argument, and are allowed on free functions, associated functions, methods, and closures.

```
fn greet(name: string, greeting: string = "Hello"): string {
    return greeting ~ ", " ~ name ~ "!";
}

greet("Ada");        // "Hello, Ada!"
greet("Ada", "Hi");  // "Hi, Ada!"
```

---

## 5. Types, structs, enums

```
// structural struct — value type, structural equality
struct Item { price: float; qty: int }

// plain enum (equivalent to PHP's backed enum)
enum Status: string {
    Pending = "pending";
    Paid = "paid";
    Refunded = "refunded";
}

// algebraic enum — variants carry data
enum OrderError {
    Empty;
    NegativePrice(index: int);
}
```

**Packed value types** (architecture §3.1). A struct whose fields are all primitives can be marked `packed` to get an unboxed, contiguous, value-semantics layout — no header, no shape, passed by value. This is what makes SIMD-amenable types (`Vec3`, `Quat`, `Mat4`, colors) fast, and it pairs with operator traits (§9.6) for an elegant surface:

```
packed struct Vec3 { x: f32; y: f32; z: f32 }
// Vec3 implements Add/Sub/Mul (operator traits), so:
position = position + velocity * dt;       // reads naturally, lays out flat

// A List of a packed type is a flat contiguous buffer, not an array of pointers —
// the layout games / numerics / ECS want.
points: List<Vec3> = [ Vec3 { x: 0, y: 0, z: 0 }, ... ];
```

Regular (non-`packed`) records and classes remain shaped, heap-allocated dynamic objects (§6); `packed` is the opt-in for the numeric/throughput case.

**The dynamic escape (`dyn`) and checked narrowing.** Every expression has an inferable static type; the single sanctioned dynamic boundary is the nameable top type `dyn` (spelled `dyn` or `Any`). Every type *widens* into it implicitly (`T` is a `dyn`), but narrowing back *out* is never implicit — you ask for it explicitly with `x.as<T>()`, which returns `?T`: `some(x)` if the runtime value is a `T`, `none` if not.

```
n = x.as<int>() ?? 0;               // narrow, then unwrap with a fallback
```

The check is on the **head constructor** — generics are erased, so `x.as<List<int>>()` tests "is a list" and trusts the element type from the annotation. Narrowing a value whose static type is already concrete (there is nothing dynamic to narrow) is a compile error (`E0028`). This is the only place runtime type dispatch survives; everywhere else the static type is known.

**Testing a type and narrowing (`is`).** When you only need a yes/no test, `x is T` is a `bool` — "is the runtime value a `T`?" (the same head-constructor check as `.as<T>()`, without the `?T` wrapper). Unlike `.as<T>()`, a test is well-formed even on an already-concrete value. An `is` test that guards a block or a `match` arm also **narrows**: inside the guard, the value is seen at the tested type, so no re-cast is needed.

```
fn describe(x: dyn): string {
    if x is int    { return "int ${x}"; }      // x is an `int` in here
    if x is string { return "len ${len(x)}"; }
    return "other";
}
```

**Union types (a *closed* `dyn`).** A union `A | B` is a `dyn` whose membership is a static, finite set — written only where you declare it, never produced by inference. A value of any member widens into it; you narrow back out with `.as<T>()`, `is`, or an `is`-pattern `match`. Because the member set is *closed*, a `match` with an `is T` arm per member is **exhaustive with no `_`** — the closed-world guarantee a union buys over `dyn` (a `dyn` match, being open, still needs a `_`).

**Abstract kind-types (`Enum` · `Struct` · `Class`).** Each is the supertype of every declared type of that kind — `Enum` accepts any enum value, `Struct` any struct, `Class` any class instance (the PHP `UnitEnum` / Java `java.lang.Enum` / C# `System.Enum` model). They sit between a concrete type and `dyn`: a concrete value widens in implicitly (`Color.Red` is an `Enum`), and you narrow back out with `is` / `.as<T>()` (`x is Enum` is a runtime kind test; `x.as<WebRole>()` recovers the concrete enum). They are **abstract** — no value *has* a kind-type at runtime (every value is a concrete enum/struct/class); a kind-type appears only in a static position (a field, parameter, or return), as a bound weaker than a concrete type but stronger than `dyn`. A `match` over a kind-typed value is open, so it needs a `_`. (This is what `roles_of()` returns each binding's `role` as — "some enum.")

```
fn label(x: int | string): string {
    return match x {
        is int    => "number ${x}",   // x narrowed to int
        is string => "text ${x}",     // x narrowed to string
    };
}

label(42);            // ok — int is a member
label("hi");          // ok — string is a member
// label(true);       // compile error: bool is not a member of `int | string`
```

`|` is the loosest type combinator, so `?A | B` reads as `(?A) | B`, and a union may appear inside generics (`List<int | string>`). Unions carry no runtime cost — the value is just its concrete `int`/`string`. Prefer a single concrete type, `dyn`, or a tagged enum/`Result`/`Option`; reach for a union only when a bounded-`dyn` is genuinely what you mean. (Intersection is expressed as trait bounds — `<T: Comparable + Display>` — not a first-class `A & B`.)

**Conditional expressions (`if…then…else`) and `??=`.** Statement `if` uses braces (`if c { … }`); the *expression* form uses `then`/`else` and yields a value: `y = if c then a else b` (and `if x is int then a else b` narrows `x` in the `then` arm). It is sugar for a two-arm `match`. For the common "default a variable if it is absent" case, `x ??= y` is the coalescing assignment — sugar for `x = x ?? y`, so it fills `x` only when it is `none` and skips evaluating `y` otherwise.

---

## 6. Classes

Fields are declared in the body. There is **no special constructor**: objects are created via the all-fields literal (`Order { ... }`, which must set every field), and `new` is just the conventional name for the most common associated function that returns `Self`. A type may have any number of named constructors as equals. Immutable-by-default fields, `.` access throughout.

```
class Order {
    id: int                         // immutable field (no keyword)
    customer: User
    items: List<Item>
    mut status: Status              // mutable field

    // `new` is convention, not a keyword — an ordinary function returning Self.
    fn new(id: int, customer: User, items: List<Item>): Order {
        return Order {              // all-fields literal: every field must be set
            id: id,
            customer: customer,
            items: items,
            status: Status.Pending,
        };
    }

    // Multiple named constructors are equals, not workarounds.
    fn draft(customer: User): Order {
        return Order { id: 0, customer: customer, items: [], status: Status.Pending };
    }

    // A fallible constructor — impossible in PHP. Inferred as a constructor
    // by tooling because it returns Result<Self, _> with no `self` receiver.
    fn parse(text: string): Result<Order, ParseError> {
        // ...
        return Ok(Order { id: 1, customer: guest(), items: [], status: Status.Pending });
    }

    fn total(): float {
        return items |> map(fn(it) => it.price * it.qty) |> sum();
    }

    fn label(): string {
        return match status {
            Status.Pending  => "Order #{id} awaiting payment",
            Status.Paid     => "Order #{id} paid: {customer.name}",
            Status.Refunded => "Order #{id} refunded",
        };
    }
}

a = Order.new(1, customer, items);
b = Order.draft(customer);
c = Order.parse(input)?;
```

**Creation rules:**
- The **all-fields literal** (`Order { ... }`) is the one creation primitive; the compiler requires every field to be set, guaranteeing full initialization.
- The literal is **private by default** for types that enforce invariants (outsiders must go through constructors like `parse`); it can be made **public** for plain data records where direct construction is fine.
- **Constructors are inferred, not marked:** an associated function (no `self` receiver) returning the enclosing type — optionally wrapped in `Result`/`Option` — is a constructor. The LSP labels and groups them with no annotation. A function returning `Self` but taking a `self` receiver (e.g. `fn with_status(self, s: Status): Order`) is a *transformation*, not a constructor.

**Structural update (clone-with-changes).** Because objects are immutable by default, "the same value with one field changed" is a constant need. The literal extends with a `..` spread:

```
a = Money.new(500, USD);
b = Money { amount: 300, ..a };     // new Money; amount overridden, currency from a
```

The spread fills every field you do not name, so the full-initialization guarantee still holds, and it respects the literal's visibility. It is **shallow by default** (safe, because immutability means shared substructure cannot be mutated); deep duplication is opt-in via `Clone` (`..a.deep_clone()`), never silent. This is the functional-update primitive (Rust `..old`, Elm `{ old | ... }`, Kotlin `.copy(...)`) unified into the creation literal rather than a separate feature. Because the spread names exactly the changed fields, change-tracking (e.g. for an ORM) is structurally explicit.

**Field assignment (`x.f = v`).** A field declared `mut` can be assigned directly — `order.status = Status.Paid` (and the compound forms `count += 1`, `name ??= "anon"`). This is the in-place counterpart to the spread update: it requires both that the field is `mut` (assigning an immutable field is `E0033` — use the spread instead) and that the binding `x` is `mut` (it is a reassignment of `x`). Assignment keeps **value semantics**: a uniquely-owned instance is mutated in place, but an aliased one is copied first, so `b = a; a.f = v` never disturbs `b`. Mutation never surfaces as shared state — the runtime mutates in place only when it can prove the instance is uniquely owned, which is also what makes it O(1) in the common accumulator loop.

---

## 7. Collections

Distinct list, map, and set literals and types.

```
nums: List<int> = [1, 2, 3];                         // list
prices: Map<string, float> = {"usd": 1.0, "eur": 0.92};  // map (string keys; keys are expressions)
tags = #{"a", "b", "a"};                             // set literal → {"a", "b"} (sorted, de-duplicated)
empty = #{};                                         // the empty set (an empty `{}` is the empty map)

doubled = nums |> map(fn(n) => n * 2);
```

A set is an ordered, de-duplicated collection of a single orderable primitive (int, float, or string). The `#{...}` literal is sugar for `[...].to_set()`; sets support `contains`/`union`/`intersection`, `len`, and `for` iteration in sorted order. Maps are string-keyed and their keys are *expressions* evaluated to strings (`{key: 1}` uses the value of `key`), so an anonymous *struct* is written with its type name (`Point { x: 1 }`), not a bare brace.

---

## 8. Control flow

Parens around conditions are optional noise and omitted; braces required. `for ... in` with optional index destructuring.

```
if items.count() == 0 {
    echo "empty";
} else if items.count() == 1 {
    echo "one";
} else {
    echo "many";
}

for item in items {
    echo item.price;
}

for (i, item) in items.enumerate() {
    echo "{i}: {item.price}";
}
```

---

## 9. The powerful spine (opt-in)

These are the features that justify the language existing. They are opt-in in the sense that simple code uses them with little ceremony (a recoverable error is `do_thing()?`, absence is `?T`); they are *not* optional in the sense of having a less-safe parallel mechanism to fall back to. There is one error hierarchy (§9.5), one nullability story, one way to mean each thing.

### 9.1 `Result`, `Option`, and `?`

```
fn validate(items: List<Item>): Result<void, OrderError> {
    if items.count() == 0 {
        return Err(OrderError.Empty);
    }
    for (i, item) in items.enumerate() {
        if item.price < 0 {
            return Err(OrderError.NegativePrice(index: i));
        }
    }
    return Ok();
}

fn place(items: List<Item>, customer: User): Result<Order, OrderError> {
    validate(items)?;   // `?` returns the Err from here if validate failed
    return Ok(Order {
        id: next_id(),
        customer: customer,
        items: items,
    });
}
```

### 9.2 Exhaustive `match` with destructuring

```
fn handle(items: List<Item>, customer: User): string {
    return match place(items, customer) {
        Ok(order)  => order.label(),
        Err(error) => match error {
            OrderError.Empty            => "Cannot place an empty order",
            OrderError.NegativePrice(i) => "Item {i} has a negative price",
        },
    };
}
```

`match` is exhaustive: omitting a variant is a compile error, with the missing case named.

### 9.3 Pipeline operator

```
report = orders
    |> filter(fn(o) => o.status == Status.Paid)
    |> map(fn(o) => o.total())
    |> sum();
```

### 9.4 Async and concurrency

`async`/`await` is the everyday tool for I/O-bound work; isolates + channels are the escalation for CPU-bound parallelism (architecture §7, §7.1). Async functions return a future; `await` suspends without blocking other tasks in the isolate. Errors flow through `await` with `?` like any other `Result`.

```
async fn fetch_user(id: int): Result<User, HttpError> {
    response = http.get("/users/{id}").await?;   // `?` propagates through await
    return response.json::<User>();
}
```

Concurrency is **structured**: a `concurrent { }` block scopes child tasks — it does not complete until all children do, child errors propagate out *at the block boundary*, and cancellation cascades. No orphaned tasks, no leaked errors.

The block body is **ordinary code** — normal statements, `if`, loops, locals — with one added capability: `spawn` is legal here and launches a tracked child of the block. The body runs sequentially top-to-bottom; only the *spawned* tasks run concurrently. `spawn` starts a task and immediately continues without waiting; the closing `}` is the barrier that waits for all children.

```
async fn load_dashboard(uid: int): Result<Dashboard, HttpError> {
    concurrent {
        user   = spawn fetch_user(uid);     // child task, starts immediately
        orders = spawn fetch_orders(uid);   // runs concurrently with the above
    }
    // Past the `}` barrier: both children have completed. Any child error
    // already propagated out of the block (so on failure the function
    // returned here), so the bindings are ready values — no await, no `?`.
    return Ok(Dashboard { user: user, orders: orders });
}
```

Because the body is ordinary code, conditional and loop-driven spawning work naturally — `for id in ids { spawn fetch(id); }` spawns N children, all joined at `}`.

**`spawn` requires an enclosing scope.** It is only legal inside a structured-concurrency scope (a `concurrent { }` block, or an `async fn` body that establishes one); `spawn` with no owning scope is a *compile error*. This is what enforces the no-orphans guarantee by construction — unlike Go's `go` or a dangling async call, a task cannot be launched without an owner to join it. Genuinely long-lived background work (a queue worker, the p2p node, a scheduler) is *not* an orphan and does not use block-scoped `spawn`; it is an explicitly runtime/isolate-owned task (architecture §7.1), so "spawn must have an owning scope" stays absolute.

**Per-child results when you want them.** The default (errors auto-propagate at the barrier) is the common case. When a child's failure should be *tolerated or inspected* rather than propagated — "fetch both, but a missing `orders` is acceptable" — `spawn` yields a handle whose result is examined after the block:

```
concurrent {
    user   = spawn fetch_user(uid);
    orders = spawn.try fetch_orders(uid);   // handle: result inspected, not auto-propagated
}
return Ok(Dashboard {
    user: user,                              // auto-propagated; guaranteed present here
    orders: orders.result.or(Orders.empty()), // inspect the tolerated child's Result
});
```

```
// Library combinators over the same machinery, for homogeneous tasks:
results = all([fetch_user(1), fetch_user(2), fetch_user(3)]).await?;  // all, or first error
winner  = race([primary(), fallback()]).await?;                       // first to finish
```

CPU-bound work goes to a worker isolate via a channel rather than blocking the async scheduler:

```
async fn render(scene: Scene): Image {
    return workers.send(scene).await;   // offload to a worker isolate, await the result
}
```

**Fire-and-forget that outlives the request** uses the *same* scope primitive at a longer lifetime: an app-lifetime `TaskScope`, **injected** (not an ambient global), spawned into so the handler returns immediately:

```
fn handle_signup(req: Request, tasks: TaskScope): Response {
    user = create_user(req)?;
    tasks.spawn(fn() => send_welcome_email(user));   // owned by the app scope; outlives this handler
    return Response.ok();                            // returns now; email sends later
}
```

`tasks` is an ordinary injected dependency, as explicit as a DB handle — no magic global. `spawn` still requires an owning scope (here the app-lifetime `TaskScope` rather than a `concurrent` block), so the task is owned, not orphaned, and the app scope drains on shutdown. A fire-and-forget task handles its own errors (its fn returns `()`), since there is no caller left to receive a `Result`. Workers, durable job queues, and schedulers are framework/first-party-extension patterns built on `TaskScope`, not language constructs (architecture §7.2).

The discipline (architecture §7.1): `async` for I/O, isolates for parallelism, shared-memory threading never exposed, concurrency always structured so lifetimes and errors are bounded — and `spawn` always owned by a scope, whether block-lifetime (`concurrent { }`) or app-lifetime (an injected `TaskScope`).

### 9.5 Graceful descent

"Scaling down" means *less ceremony*, not a less-safe parallel mechanism. The simple form of a feature is still the principled one:

```
// Absence is Option, written as ?T. `none`/`some` are the values.
fn find(id: int): ?User {
    // ...
    return none;
}

// A recoverable error is a one-liner with `?` — the easy path is the safe path.
fn load(id: int): Result<User, LoadError> {
    user = find(id) ?? return Err(LoadError.NotFound);   // `??` handles the None case
    return Ok(user);
}

// A plain enum with no associated data is just an enum — no ceremony required.
enum Color { Red; Green; Blue; }

// Exceptions exist only for the genuinely exceptional (unrecoverable / "can't happen").
// This panics and unwinds the isolate; it is NOT the everyday error path.
fn invariant_broken() {
    panic("unreachable: validated upstream");
}
```

There is one error hierarchy: `Result`/`Option` for everything recoverable (the everyday path), `panic` for the truly unrecoverable. No ambient `null`, no co-equal throw-for-everyday-errors mechanism — collapsing those is deliberate, not an oversight.

### 9.6 Operators and built-in protocols (traits)

A single mechanism replaces *all* of PHP's magic methods plus the operator overloading and object comparison PHP never had: a class implements a **trait**, and operators and native-type behavior dispatch through it. There is no separate "magic method" concept. (Architecture doc §9.2 covers the full mapping and the mechanism; this is the surface syntax.)

Implementing a trait lights up its operator or behavior:

```
class Money {
    amount: int
    currency: Currency

    fn new(amount: int, currency: Currency): Money {
        return Money { amount: amount, currency: currency };
    }

    impl Equatable {
        fn eq(other: Money): bool {
            return amount == other.amount && currency == other.currency;
        }
    }

    impl Add {                                  // lights up the + operator
        fn add(other: Money): Money {
            return Money(amount + other.amount, currency);
        }
    }

    impl Display {                              // lights up echo / interpolation
        fn to_string(): string { return "{amount} {currency}"; }
    }

    impl Index {                                // lights up a[i] (ArrayAccess equivalent)
        fn get(key: string): int { /* ... */ }
    }
}

a = Money(500, USD);
b = Money(300, USD);

total = a + b;       // Add.add
same  = a == b;      // Equatable.eq
echo a;              // Display.to_string
```

**Default methods derive related operators.** Implement `compare` and `<`, `<=`, `>`, `>=` all work:

```
impl Comparable {
    fn compare(other: Money): Ordering {
        return amount.compare(other.amount);
    }
}
// a < b, a >= b, etc. now all work
```

**Derivation for the common value-object case** — no hand-written body needed. Code generation is its own directive, `@derive(...)` (the `@` sigil means "the compiler generates something"), kept distinct from the `#[...]` data attributes of §9.7:

```
@derive(Equatable, Comparable, Display, Clone)
class Point {
    x: int
    y: int

    fn new(x: int, y: int): Point { return Point { x: x, y: y }; }
}
// field-wise ==, ordering, string form, and copy are synthesized
```

**Trait coherence** — a type implements each trait **at most once**, counting a `@derive(T)`, an in-body `impl T { }`, and a standalone `impl T for Type { }` all as implementations. Deriving a trait *and* writing an `impl` for the same trait, deriving it twice, or writing two `impl` blocks for it is a compile error (`E0027`): the two implementations would compete for the same operator/protocol dispatch. The complementary orphan rule is enforced without a foreign-impl syntax — every trait is built-in, an in-body `impl` block lives inside the class it applies to, and a standalone `impl T for Type { }` must target a type declared in the **same module** (else `E0013`), so you can never implement a trait for a type you do not own. Coherence is what lets a generic bound like `<T: Comparable>` be checked with a single, unambiguous answer to "does `T` implement `Comparable`".

**Fallible operators** use the `Try*` variants returning `Result`, so an operation that can fail is a typed error rather than a crash:

```
impl TryAdd {
    fn try_add(other: Money): Result<Money, MoneyError> {
        if currency != other.currency {
            return Err(MoneyError.CurrencyMismatch);
        }
        return Ok(Money(amount + other.amount, currency));
    }
}

result = a.try_add(b)?;     // `?` propagates the error; bare `+` is reserved for infallible Add
```

**Dynamic interception** (PHP's `__get`/`__set`/`__call`) is the same mechanism:

```
class Proxy {
    impl Members {                              // __get / __set equivalent
        fn get(name: string): any { /* ... */ }
        fn set(name: string, value: any) { /* ... */ }
    }
    impl DynamicCall {                          // __call equivalent
        fn call(method: string, args: List<any>): any { /* ... */ }
    }
}
```

Common protocols and the PHP magic they replace: `Equatable` (`==`), `Comparable` (`< <= > >=`), `Add`/`Sub`/`Mul`/`Div` (`+ - * /`), `Concat` (`~`), `Index` (`ArrayAccess`), `Display` (`Stringable`), `Length` (`Countable`), `Iterable` (`IteratorAggregate`), `Callable` (`__invoke`), `Members` (`__get`/`__set`), `DynamicCall` (`__call`/`__callStatic`), `Clone` (`__clone`), `Serialize<Format>`. Construction is *not* a trait and not a special form — it is ordinary associated functions returning `Self` (`new`, `parse`, ...; see §6). Destruction is *not* a trait either, but for a different reason: it is the one hook invoked by the runtime (the GC) rather than by user code, so it stays a distinct `destruct` language construct, not directly callable (see architecture §9.2).

### 9.7 Attributes and reflection

No comptime or user-defined macros (see architecture §9.13). Three surfaces cover the cases PHP uses runtime reflection for. Code generation and data attributes are **two different operations with two different sigils**, so neither overloads the other: `@derive(...)` is compile-time codegen (closed, compiler-provided); `#[...]` attaches a data attribute (open, user-definable). One-line model: **`@` = the compiler generates something; `#[...]` = metadata attached** (PHP-attributes model).

**Built-in derives** (`@derive`) handle shape-based codegen. You apply them; you do not write new ones (the stdlib/compiler implements them):

```
@derive(Serialize<Json>, Equatable, Clone)
class User {
    name: string
    age: int
    fn new(name: string, age: int): User { return User { name: name, age: age }; }
}
```

A derive may carry **generic type arguments**: `Serialize<Format>` is the format-parameterized serializer (`@derive(Serialize<Json>)` synthesizes the structural `to_json`), the format chosen from a blessed vocabulary (`Json` to start). Supplying the wrong number of arguments — `@derive(Serialize)`, `@derive(Comparable<int>)` — is an arity error (E0014); an unknown format is E0013. The other derivable traits (`Equatable`, `Comparable`, `Display`, `Clone`) are nullary.

**Attributes are just structs** used in annotation position — no special construct (architecture §9.13). A struct opts in as an attribute with the **`@attribute` directive**; its `#[...]` arguments then map to the struct's fields (positional in declaration order, named by name), the one unambiguous construction a struct has — which is exactly why **attributes are structs, not classes** (a class has only convention-named constructors, with no canonical one to call). A `#[...]` may attach to any declaration site — a type, a function, a method, a field/property, or an enum variant. Placement can be constrained by listing the permitted **target kinds** in the directive (`Struct`, `Class`, `Enum`, `Function`, `Method`, `Field`, `Variant`); a bare `@attribute` attaches anywhere. A misplaced use is a compile error (E0030):

```
@attribute(Method, Function)                   // opt-in + constrain placement
struct Route { path: string }                  // a struct; #[Route(...)] fills `path`

class UserController {
    #[Route("/users")]                         // OK — Method is permitted
    fn index(): Response { ... }
}

#[Route("/x")]                                 // E0030 — Route does not attach to a type
struct User { id: int }
```

A struct can still carry *behavior* via a standalone `impl SomeTrait for Route {}` (so "structs only" costs no expressiveness); and a bare `@attribute` (no kinds) is the common "attaches anywhere" case.

**Attribute arguments are a constant literal tree.** Beyond scalars, an argument may be a list, map, set, enum value, or a nested struct literal — composed arbitrarily — plus a **type reference** (a bare type name, like C# `typeof(Foo)`). The one rule is *no comptime*: an argument is materialized at manifest-build time without running user code, so it is literals and compositions of literals only — never an expression (`1 + 2`), a call, or a name read of runtime state. The checker type-checks the whole tree recursively against the attribute's field types (E0007), and a bare name must resolve to a real type (E0013):

```
@attribute
struct Endpoint {
    methods: List<Method>                 // a list of enum values
    limits:  Map<string, int>             // a map
    tags:    Set<string>                  // a set (#{...})
    fallback: Limits                      // a nested struct literal
    codec:   Type                         // a type reference
}

#[Endpoint(
    methods: [Method.Get, Method.Post],
    limits:  { "rps": 100, "burst": 200 },
    tags:    #{"public", "cached"},
    fallback: Limits { rps: 1, burst: 2 },
    codec:   JsonCodec,
)]
struct Users { id: int }
```

A bare name disambiguates cleanly: `Enum.Variant` (or a built-in `Ok(5)`/`none`) is an **enum value**, while an unqualified name is a **type reference** — materialized as the reflection `Type` (`type_of`'s result type), so it is matchable (`match codec { Type.Named(n, _) => … }`) *and* operational: `invoke` accepts a `Type` value as its receiver, dispatching the named type's associated function, so a stored type-ref is constructible, not just inspectable.

> Design note: the placement rule is a static, declarative list of kinds — *not* a user-evaluated `valid_target(t: Target): bool` predicate. Enforcing an arbitrary predicate would require executing user code at compile time (the project deliberately avoids comptime), and the target kinds are a fixed, closed set, so they live as plain identifiers the checker reads. A predicate over a target's *return type* would be a future enhancement, gated on comptime.

**Discovery and registration are a manifest query**, not a runtime scan. The compiler already indexed every attribute; consumers read that index, compiled in as a static table:

```
routes = attributes_of::<Route>();    // static, compiler-built; no runtime reflection
for r in routes {
    register(r.path, r.target);
}
```

The same index powers the LSP and static analysis ("show every `#[Route]`", "who consumes `#[Entity]`", jump-to-all-usages). Attributes carry no behavior beyond being structs — they reduce to structs + traits + the manifest, all of which exist for other reasons.

**Runtime reflection** exists for the genuinely dynamic minority, unified with the type system and fallible by design:

```
match type_of(value) {
    Type.Struct(name, _) => "struct ${name}",
    Type.Enum(name, _)   => "enum ${name}",
    Type.Class(name, _)  => "class ${name}",
    Type.List(elem)      => "list",
    Type.Int             => "int",
    _                    => "other",
}
// `type_of` reports the value's concrete type, distinguishing the three nominal kinds
// (`Type.Enum`/`Type.Struct`/`Type.Class`) so a consumer can branch on kind from the result alone.
// The abstract `Enum`/`Struct`/`Class` *types* are the static-bound counterpart (`roles_of()`'s
// `role` is `Enum`); the runtime kind test on a value is `x is Enum`.

// By-name invocation — the single fallible primitive. The name is a runtime string; the result is
// `Result<dyn, dyn>`. The receiver is a value (→ instance method) or a type (→ associated function,
// including a constructor — "construct" is just a convention, not a capability).
result   = invoke(order, method_name, args)?;   // call a method by name      → Result
instance = invoke(some_type, "new", args)?;     // construct-by-name can fail  → Result
```

Introspection (`type_of`, `attributes_of`) is read-only; invocation is explicitly fallible — an unknown name or wrong arity is a runtime `Result.Err`, never a static error. Runtime reflection is opt-in per type (capability-gated): reflectable types become tree-shaking roots, so unused metadata is eliminated from AOT binaries (architecture §9.8.1).

**Semantic role tags** let an attribute confer a typed architectural *role* on whatever it annotates — declared once on the attribute, inherited by every use. The role vocabulary is **user-extensible**: the language ships a built-in `Semantic` enum, and any enum a project marks `@semantic` becomes role-eligible too. An attribute carries a `@role(Enum.Variant)` directive naming a fieldless variant of a `@semantic` enum:

```
@semantic                                          // promote a framework's own roles
enum WebRole { Controller, Middleware, ErrorHandler }

@attribute(Function, Method)
@role(Semantic.EntryPoint)                         // a built-in role
struct Route { path: string }

@attribute
@role(Semantic.Sink)
struct Persist { table: string }

@attribute(Function, Method)
@role(WebRole.Controller)                          // a framework-specific role
struct Page { path: string }

// the built-in vocabulary, implicitly @semantic
@semantic enum Semantic { EntryPoint, PersistenceBoundary, TrustBoundary, Sink, Layer }
```

The compiler indexes `(declaration, role)` at manifest-build time (zero runtime cost), and `roles_of()` surfaces it as a `List<RoleBinding>` (each `{ target: string, role: Enum }` — `role` is the abstract `Enum` kind, since a binding's role may be any `@semantic` enum) so the dependency graph becomes queryable in architectural terms — "every entry point," "does this trust boundary reach a sink":

```
for binding in roles_of() {
    match binding.role {                           // Enum is open, so it needs a `_`
        Semantic.EntryPoint => register_entry(binding.target),
        Semantic.Sink       => audit_sink(binding.target),
        WebRole.Controller  => register_controller(binding.target),
        _                   => skip(),
    }
}
```

`@role(...)` is declarative — a fixed `Enum.Variant` name, **not** a user-evaluated `fn role(): Enum` (which would require compile-time execution of user code; the project avoids comptime, exactly as for attribute placement). Multiple roles may tag one declaration (each becomes its own binding). Only a struct marked `@attribute` may carry `@role` (the role rides on what the attribute attaches to; otherwise E0031), a class/enum cannot (attributes are structs only), and the named variant must be **fieldless** — a *parameterized* role (`Layer(name)`), whose payload would be evaluated per use site, is deferred as a comptime enhancement. `@semantic` marks enums only (on a struct/class it is E0031). Agents query the labeled graph through MCP tools (`list_roles`/`trace_from`/`flows_between`); architecture §12.7.

---

## 10. Namespaces and imports

```
namespace App.Orders;

use App.Models.User;
use App.Billing.{Invoice, Receipt};   // grouped import
```

---

## 11. Reactivity (signals)

Server-side reactivity as a language primitive (see architecture doc §9.4).

```
count = signal(0);
doubled = computed(fn() => count.get() * 2);

effect(fn() => {
    echo "count is now {count.get()}";   // re-runs when count changes
});

count.set(5);   // triggers doubled recompute and the effect
```

---

## 12. Testing

Testing is first-class and built into the toolchain (`lang test`); the runner reuses the same infrastructure as the language's own conformance suite (architecture §11.3). A test is a named block (or a `#[test]` function) with assertions that produce good diffs. Async tests (§9.4) are first-class.

```
test "order total sums line items" {
    order = Order.new(1, customer, [
        Item { price: 10.0, qty: 2 },
        Item { price: 5.0, qty: 1 },
    ]);
    assert order.total() == 25.0;
}

test "validate rejects empty orders" {
    assert validate([]) == Err(OrderError.Empty);
}

#[test]
async fn fetches_user() {
    user = fetch_user(1).await?;
    assert user.id == 1;
}
```

---

## 13. Logging and agent tools

**Structured logging** (architecture §12.1) — events carry typed fields, not interpolated strings, so they are queryable:

```
log.info("order placed", { order_id: order.id, total: order.total, user: user.id });
log.error("payment failed", { order_id: order.id, reason: err });
```

Where logs go is a driver, configured at startup (stdout / file / Loki / Datadog / embedded store), not specified at the call site.

**Registering a custom agent tool** (architecture §12.4) — free-form *what*, standard typed *how*. A tool has a name, typed params, a typed return, and a description; the MCP server exposes it to agents uniformly:

```
#[agent_tool(desc: "Inspect an order by id")]
fn inspect_order(id: int): OrderReport {
    // ... ordinary code; typed params and return are the tool's schema
}
```

The tool is dev-only by default; production exposure requires adding it to an explicit allowlist (architecture §12.4). Framework authors register domain tools the same way (`list_routes`, `query_schema`, etc.) — the language fixes the *mechanism*, never the *set* of tools.

---

## 14. A complete small program

```
namespace App.Orders;

use App.Models.User;

struct Item { price: float; qty: int }

enum Status: string {
    Pending = "pending";
    Paid = "paid";
    Refunded = "refunded";
}

enum OrderError {
    Empty;
    NegativePrice(index: int);
}

class Order {
    id: int
    customer: User
    items: List<Item>
    mut status: Status

    fn new(id: int, customer: User, items: List<Item>): Order {
        return Order { id: id, customer: customer, items: items, status: Status.Pending };
    }

    fn total(): float {
        return items |> map(fn(it) => it.price * it.qty) |> sum();
    }

    fn label(): string {
        return match status {
            Status.Pending  => "Order #{id} awaiting payment",
            Status.Paid     => "Order #{id} paid: {customer.name}",
            Status.Refunded => "Order #{id} refunded",
        };
    }
}

fn validate(items: List<Item>): Result<void, OrderError> {
    if items.count() == 0 {
        return Err(OrderError.Empty);
    }
    for (i, item) in items.enumerate() {
        if item.price < 0 {
            return Err(OrderError.NegativePrice(index: i));
        }
    }
    return Ok();
}

fn place(items: List<Item>, customer: User): Result<Order, OrderError> {
    validate(items)?;
    return Ok(Order { id: next_id(), customer: customer, items: items, status: Status.Pending });
}
```

---

## 15. Design rules for future syntax

1. **Prefer the clear, conventional spelling unless there is a real reason to diverge.** Where PHP (or any mainstream language) has a sensible, unambiguous construct, keep it; change a thing only when it fixes a genuine problem — an ambiguity, a historical accident, or a daily papercut (`$`, `->`, `\`, dual-purpose arrays, `=>` overload). Novelty for its own sake is a cost, not a feature.
2. **Every advanced feature needs a readable surface and a simpler fallback.** If a feature can only be expressed in a way that is hard to read, the terseness is not worth it. Power should be reachable, not mandatory.
3. **Uniform rules over precise ones.** One rule ("nothing changes unless `mut`") beats two context-dependent rules. Uniformity is worth more than precision for teachability.
4. **Keep redundant handholds** (`return`, braces, semicolons) even where optional — they keep the powerful constructs legible.
