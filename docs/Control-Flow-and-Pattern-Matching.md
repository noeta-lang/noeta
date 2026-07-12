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

```noeta check
fn classify(p: (int, int)): string {
    return match p {
        (0, 0) => "origin",
        (0, y) => "y-axis at ${y}",
        (x, 0) => "x-axis at ${x}",
        (x, y) => "at ${x},${y}",
    }
}

echo match place(items, customer) {
    Ok(order)  => order.label(),
    Err(error) => "failed",
}
```

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
