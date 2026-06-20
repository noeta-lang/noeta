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
| `"Hello $name"` | `"Hello {name}"` | Explicit interpolation without the sigil. |
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
greeting = "Hello {name}";              // brace interpolation
path = "users/" ~ id ~ "/profile";      // `~` concatenation
raw = `multi
line`;                                   // backtick multiline (also supports {expr})
```

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

---

## 5. Types, records, enums

```
// structural record — value type, structural equality
type Item = { price: float, qty: int };

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

**Packed value types** (architecture §3.1). A record whose fields are all primitives can be marked `packed` to get an unboxed, contiguous, value-semantics layout — no header, no shape, passed by value. This is what makes SIMD-amenable types (`Vec3`, `Quat`, `Mat4`, colors) fast, and it pairs with operator traits (§9.6) for an elegant surface:

```
packed type Vec3 = { x: f32, y: f32, z: f32 };
// Vec3 implements Add/Sub/Mul (operator traits), so:
position = position + velocity * dt;       // reads naturally, lays out flat

// A List of a packed type is a flat contiguous buffer, not an array of pointers —
// the layout games / numerics / ECS want.
points: List<Vec3> = [ Vec3 { x: 0, y: 0, z: 0 }, ... ];
```

Regular (non-`packed`) records and classes remain shaped, heap-allocated dynamic objects (§6); `packed` is the opt-in for the numeric/throughput case.

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

---

## 7. Collections

Distinct list and map literals and types.

```
nums: List<int> = [1, 2, 3];
prices: Map<string, float> = {"usd": 1.0, "eur": 0.92};

doubled = nums |> map(fn(n) => n * 2);
```

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
fn find(id: int): User? {
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

**Derivation for the common value-object case** — no hand-written body needed:

```
#[derive(Equatable, Comparable, Display, Clone)]
class Point {
    x: int
    y: int

    fn new(x: int, y: int): Point { return Point { x: x, y: y }; }
}
// field-wise ==, ordering, string form, and copy are synthesized
```

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

Common protocols and the PHP magic they replace: `Equatable` (`==`), `Comparable` (`< <= > >=`), `Add`/`Sub`/`Mul`/`Div` (`+ - * /`), `Concat` (`~`), `Index` (`ArrayAccess`), `Display` (`Stringable`), `Length` (`Countable`), `Iterable` (`IteratorAggregate`), `Callable` (`__invoke`), `Members` (`__get`/`__set`), `DynamicCall` (`__call`/`__callStatic`), `Clone` (`__clone`), `Serialize`/`ToJson`. Construction is *not* a trait and not a special form — it is ordinary associated functions returning `Self` (`new`, `parse`, ...; see §6). Destruction is *not* a trait either, but for a different reason: it is the one hook invoked by the runtime (the GC) rather than by user code, so it stays a distinct `destruct` language construct, not directly callable (see architecture §9.2).

### 9.7 Attributes and reflection

No comptime or user-defined macros (see architecture §9.13). Three surfaces cover the cases PHP uses runtime reflection for.

**Built-in derives** handle shape-based codegen. You apply them; you do not write new ones (the stdlib/compiler implements them):

```
#[derive(ToJson, Equatable, Clone)]
class User {
    name: string
    age: int
    fn new(name: string, age: int): User { return User { name: name, age: age }; }
}
```

**Attributes are just records** used in annotation position — no special construct (architecture §9.13). A record marked with the `Attribute` trait can be attached; constructing the attribute is the same constructor machinery as any value. Constraints on *where* it attaches are an ordinary trait impl, checked at compile time:

```
#[derive(Attribute)]
record Route { path: string }                 // a plain record, usable as an attribute

impl AttachableTo for Route {                  // optional: constrain placement
    fn valid_target(t: Target): bool {
        return t.is_method() && t.returns(Response);   // misuse is a compile error
    }
}

class UserController {
    #[Route("/users")]                         // constructs Route { path: "/users" }
    fn index(): Response { ... }
}
```

**Discovery and registration are a manifest query**, not a runtime scan. The compiler already indexed every attribute; consumers read that index, compiled in as a static table:

```
routes = attributes_of::<Route>();    // static, compiler-built; no runtime reflection
for r in routes {
    register(r.path, r.target);
}
```

The same index powers the LSP and static analysis ("show every `#[Route]`", "who consumes `#[Entity]`", jump-to-all-usages). Attributes carry no behavior beyond being records — they reduce to records + traits + the manifest, all of which exist for other reasons.

**Runtime reflection** exists for the genuinely dynamic minority, unified with the type system and fallible by design:

```
match type_of(value) {
    Type.Record(r)    => "record, {r.fields.count()} fields",
    Type.Enum(e)      => "enum: {e.variants}",
    Type.Primitive(p) => "primitive {p}",
}

instance = some_type.construct(args)?;     // construct-by-name can fail → Result
```

Runtime reflection is opt-in per type (capability-gated): reflectable types become tree-shaking roots, so unused metadata is eliminated from AOT binaries (architecture §9.8.1).

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

type Item = { price: float, qty: int };

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
    return Ok(Order { id: next_id(), customer: customer, items: items });
}
```

---

## 15. Design rules for future syntax

1. **Prefer the clear, conventional spelling unless there is a real reason to diverge.** Where PHP (or any mainstream language) has a sensible, unambiguous construct, keep it; change a thing only when it fixes a genuine problem — an ambiguity, a historical accident, or a daily papercut (`$`, `->`, `\`, dual-purpose arrays, `=>` overload). Novelty for its own sake is a cost, not a feature.
2. **Every advanced feature needs a readable surface and a simpler fallback.** If a feature can only be expressed in a way that is hard to read, the terseness is not worth it. Power should be reachable, not mandatory.
3. **Uniform rules over precise ones.** One rule ("nothing changes unless `mut`") beats two context-dependent rules. Uniformity is worth more than precision for teachability.
4. **Keep redundant handholds** (`return`, braces, semicolons) even where optional — they keep the powerful constructs legible.
