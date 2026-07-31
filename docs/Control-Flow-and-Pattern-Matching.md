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
| Binding | `n` — a name that is *not* a case of the scrutinee's enum; binds the whole value, matches everything, so it goes last |
| Tuple | `(0, 0)`, `(x, y)`, `(1, label, _)`, nested `(n, (s, flag))` |
| Enum | `Paid` or `Status.Paid` (payload-free), payload-binding `OrderError.NegativePrice(i)` |
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

### A bare identifier is a variant when the scrutinee's enum has one

A bare identifier in a pattern is read against the **scrutinee's own type**. If that type is an enum with a **payload-free variant of that name**, the pattern *is* that variant: it binds nothing, it matches only that case, and it counts toward exhaustiveness. Otherwise it is a **binding** — it names the whole value and matches everything.

```noeta
enum Kind { Text; Number }

fn describe(k: Kind): string {
    return match k {
        Text   => "text",             // the `Kind.Text` case — matches only it
        Number => "number",           // …and naming every case closes the match, no `_` needed
    }
}
echo describe(Kind.Number)            // number
```

The qualified spelling `Kind.Text` still works and means exactly the same thing; reach for it when the short name would be ambiguous *to a reader*. A payload-carrying variant is call-shaped (`Type.List(inner)`, `some(x)`, `Ok(v)`) and was never ambiguous either way.

Resolution is **scrutinee-directed**, which is the whole of the rule:

- a name that is a payload-free variant of some *other* enum is not this scrutinee's case — it stays a binding;
- when the scrutinee's type is `dyn`, gradual, or otherwise not a known enum, there is nothing to resolve against and every bare name is a binding;
- a nested pattern resolves against the **field's** type, not the outer scrutinee's — in `Ok(none)` the inner `none` is read against the `Ok` payload's type;
- a guard changes nothing;
- `for` loops, `let`-destructuring and other binder positions are not match arms and are unaffected.

`none` follows the same rule against the built-in `Option`, so it is the none *case* rather than a catch-all — which is why `match o { none => …, some(v) => … }` means what it reads as.

The cost of the rule: a catch-all cannot be *named* after a case of the scrutinee's enum. Writing

```noeta error
fn describe(k: Kind): string {
    return match k {
        Number => "number",
        Text => "any text at all",    // this is the `Kind.Text` case, not a catch-all named `Text`
    }
}
```

gives you two case arms, not a case and a catch-all — and if you meant to bind, rename it (`rest`, `other`, `k2`). A name that reads as a variant *is* that variant, so a reader never has to guess which one an arm meant.

### Arm order — an arm after a catch-all is dead (E0066)

An unguarded `_`, or an unguarded bare identifier that is genuinely a binding, matches every value — so any arm written after it can never run. That is **E0066**, an error: the arm is dead code no author intends, and (unlike an always-false type test) nothing in the source shows it. Put the catch-all last.

```noeta error
fn rank(n: int): string {
    return match n {
        _ => "many",
        1 => "one",      // E0066: unreachable — the `_` above matches everything
    }
}
echo rank(1)
```

A **guarded** arm (`pattern if cond`) is not a catch-all — the checker cannot prove a guard ever true — so arms after it stay reachable. Neither is a bare identifier that [resolved to a variant](#a-bare-identifier-is-a-variant-when-the-scrutinees-enum-has-one): it emits a real test, so the arms below it stay live. Only a `_`, or a bare name that is genuinely a binding, closes a `match`.

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
