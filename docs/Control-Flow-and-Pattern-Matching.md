# Control Flow & Pattern Matching

Conditionals, loops, and `match` — including the expression forms and flow-narrowing.

## `if` / `else`

Statement form, braces required:

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

> [!NOTE]
> The `{` after `if`/`while`/`for` is always the block, so a **bare struct literal cannot** lead a condition — parenthesize it: `if (Cfg { debug: true }).debug { … }`.

## `if … then … else` — the conditional expression

With the `then` keyword, `if` is an **expression** usable anywhere. It desugars to a two-arm `match`:

```noeta
fn size(n: int): string {
    return if n > 10 then "big" else "small"
}

label = if size(50) == "big" then 9 else 0   // as a binding's value
```

A `cond is T` test **narrows** the scrutinee's type inside the `then` arm:

```noeta
fn describe(x: dyn): string {
    return if x is int then "int ${x}" else "other"
}
```

## `while`

Top-tested loop:

```noeta
mut i = 0
while i < 3 {
    echo i
    i += 1
}
```

## `for … in`

Iterates lists, ranges, sets, maps, iterators, or any `Iterable`. The loop pattern may destructure a tuple:

```noeta
mut total = 0
for n in [1, 2, 3, 4] { total = total + n }
for i in 0..3 { echo i }
for (i, x) in ["a", "b"].enumerate() {   // enumerate yields (index, value) tuples
    echo "${i}:${x}"
}
```

Iterating a map yields its values; iterating a set yields elements in sorted order.

A user type iterates through the `Iterable` protocol — `iter()` returning a list — or as a **`next`-driven iterator**: an object exposing a callable `next` member (a method, or a closure-valued field) is driven `next()` → `some(x)`/`none` until exhausted (`some`/`none` and the optional type `?int` are covered in [Error Handling](Error-Handling)). `iter()` may itself return such a handle. User iteration is eager: the elements are materialized up front. Lazy, on-demand streaming belongs to the built-in `Iterator<T>` (`xs.iter()`).

```noeta
struct Gen {
    next: () -> ?int
}
fn counter(hi: int): Gen {
    mut n = 0
    return Gen { next: fn(): ?int {
        if n >= hi { return none }
        n = n + 1
        return some(n - 1)
    } }
}
for x in counter(3) { echo x }   // 0 1 2
```

## `break` and `continue`

Both work in `while` and `for`, including inside a nested `if`. `break` outside any loop is E0024.

```noeta
for n in 0..100 {
    if n == 5 { break }
    if n % 2 == 0 { continue }
    echo n         // 1 3
}
```

## `match`

`match scrut { pat => expr, … }` is an **expression**, and it is checked for **exhaustiveness** — a missing case with no `_` is E0011.

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

### Pattern forms

| Form | Example |
|---|---|
| Literal | `0`, `"a"`, `true` |
| Wildcard | `_` |
| Binding | `n` (binds the whole value) |
| Tuple | `(0, 0)`, `(x, y)`, `(1, label, _)`, nested `(n, (s, flag))` |
| Enum | `Status.Paid`, payload-binding `OrderError.NegativePrice(i)` |
| Option | `some(n)`, `none` |
| Result | `Ok(v)`, `Err(e)` |
| Type | `is int`, `is string`, `is Point` (on unions / `dyn`) |

```noeta
fn classify(p: (int, int)): string {
    return match p {
        (0, 0) => "origin",
        (0, y) => "y-axis at ${y}",
        (x, 0) => "x-axis at ${x}",
        (x, y) => "at ${x},${y}",
    }
}
echo classify((0, 4))          // y-axis at 4

fn parse_age(s: string): Result<int, string> {
    return match s.to_int() {
        some(n) => Ok(n),
        none    => Err("not a number: ${s}"),
    }
}
echo match parse_age("42") {
    Ok(n)  => "age ${n}",
    Err(e) => e,
}                              // age 42
```

### Guards

An arm may carry a **guard**: `pattern if cond => body`. The guard is a plain `bool` expression, evaluated only after the pattern structurally matches — with the pattern's bindings in scope. A `false` guard **falls through to the next arm**, exactly as a failed pattern would:

```noeta
fn check(n: int): Result<int, string> {
    if n < 0 {
        return Err("invalid: ${n}")
    }
    return Ok(n)
}

fn label(n: int): string {
    return match check(n) {
        Ok(age) if age >= 18 => "adult",
        Ok(age) if age >= 13 => "teen",
        Ok(_) => "child",         // the guarded arms fall through to here
        Err(e) => e,
    }
}

echo label(21)                 // adult
echo label(15)                 // teen
echo label(4)                  // child
```

Guards narrow nothing — narrowing stays purely pattern-driven — but they compose with `is` arms, where the guard already sees the scrutinee narrowed to the arm's type:

```noeta
fn describe(x: int | string): string {
    return match x {
        is int if x > 9 => "big int ${x}",
        is int => "int ${x}",
        is string => "len ${x.len()}",
    }
}

echo describe(12)              // big int 12
echo describe(3)               // int 3
```

A guarded arm contributes **nothing to exhaustiveness**: the checker cannot prove a guard ever true, so the arm's case stays uncovered for when the guard is false. A `match` whose only `Ok` arm is guarded is non-exhaustive (E0011) — add an unguarded arm for the case (or a `_` catch-all):

```noeta error
// E0011: non-exhaustive — `Ok(x) if x > 0` does not cover `Ok` (its guard may be false).
fn f(): Result<int, string> { return Ok(1); }
echo match f() {
    Ok(x) if x > 0 => "pos",
    Err(_) => "err",
}
```

A guard chooses an *arm*; when both outcomes share one arm, branching inside it with the `if … then … else` expression still reads well:

```noeta
n = 5
echo match n {
    0 => "zero",
    k => if k > 0 then "pos" else "neg",
}
// pos
```

### Arm bodies: expressions vs. blocks

An arm body is usually a **value expression** (`pattern => expr`) — that value becomes the `match`'s result. An arm may also be a **statement block** (`pattern => { stmts }`) for side effects that need no artificial expression. A block is a statement sequence, so — like a block-bodied function — it **produces no value** (`unit`); its statements run in the enclosing frame, so a `return` inside exits the enclosing function and `break`/`continue` target the enclosing loop.

```noeta
enum Cmd { Log; Skip; Retry; }

fn audit(c: Cmd): void { echo "audited"; }
fn handle(c: Cmd): void { echo "handled"; }

cmd = Cmd.Log
match cmd {
    Cmd.Log => { echo "logging"; audit(cmd); },   // block arm: runs for effect, yields unit
    Cmd.Skip => { },                              // empty block arm
    _ => handle(cmd),
}
```

Because a block yields no value, a block arm is only valid where the `match`'s value is **discarded** — i.e. the `match` stands in statement position. Using a block arm where the value is **consumed** (a binding RHS, an argument, a `return`, an operand) is a compile error (**E0055**): the arm would silently contribute `unit` where a value is expected. Give such an arm a value expression instead (the block's last statement is *not* its value):

```noeta error
// E0055: `2 => { … }` produces no value, but this `match` is bound to `r`.
x = 2
r = match x {
    1 => "one",
    2 => { t = "tw"; t ~ "o" },   // ✗ write `2 => { t = "tw"; t ~ "o" }` as `2 => "two"`
    _ => "many",
}
```

`{ … }` still parses as an **expression** first, so `=> {}` and `=> {"k": v}` keep their empty-map / map-literal meaning.

### Open vs. closed matching

- A **union** (`A | B`) is a *closed* world — a `match` over it is exhaustive with **no `_`** (one `is` arm per member).
- **`dyn`** is *open* — a finite set of `is T` arms can never exhaust it, so a `_` arm is required (E0011 without one).

```noeta
fn kind(x: int | string): string {
    return match x {
        is int    => "int",
        is string => "string",
    }
}
```

## Flow-narrowing

An `is` test narrows a variable's type in the block it guards:

```noeta check
struct Circle { r: float }

fn area(x: dyn): float {
    if x is Circle {
        return 3.14159 * x.r * x.r   // x is Circle here
    }
    return 0.0
}
```

Each `is` arm of a `match` narrows the scrutinee to that type. See [The Type System](Type-System) for `is`, `.as<T>()`, unions, and `dyn` in full.
