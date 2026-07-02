# Error Handling

No `null`, no exceptions. Absence and failure are ordinary values you pass, match, and propagate.

## `Option` — `?T`

An optional value is either `some(x)` or `none`. The type is written `?T`.

```lang
fn pick(hit: bool): ?int {
    if hit { return some(7) }
    return none
}

echo match pick(true) {
    some(n) => "found ${n}",
    none    => "absent",
}
```

Many stdlib operations return options — `[1, 2, 3].first()` → `some(1)`, `[].last()` → `none`, an iterator's `.next()`, a file handle's `.read_line()`, a channel's `.recv()`.

## `Result<T, E>`

A result is either `Ok(x)` or `Err(e)`. Use `Ok()` (no argument) for `Result<void, E>`.

```lang
enum OrderError { Empty; NegativePrice(index: int) }

fn validate(items: List<Item>): Result<void, OrderError> {
    if items.count() == 0 { return Err(OrderError.Empty) }
    return Ok()
}

echo match validate([]) {
    Ok()   => "valid",
    Err(e) => "invalid: ${e}",
}
```

## `?` — propagate a failure

On a `Result` or `Option`, the postfix `?` unwraps the success value, or **early-returns** the `Err`/`none` from the enclosing function. Using `?` on any other type is E0012.

```lang
fn place(items: List<Item>): Result<Order, OrderError> {
    validate(items)?                        // returns the Err here if invalid
    return Ok(Order.new(next_id(), items))
}
```

This lets you write the happy path linearly while failures short-circuit outward:

```lang
fn pipeline(path: string): Result<Report, Error> {
    raw    = fs_read(path)?      // returns Err on read failure
    parsed = parse(raw)?         // returns Err on parse failure
    return Ok(analyze(parsed))
}
```

## `??` — coalesce

`expr ?? fallback` unwraps a `some`, or evaluates the fallback for a `none`/absent value. It short-circuits — the fallback runs only when needed:

```lang
echo find(false) ?? "guest"      // "guest" if find returns none

mut present = some(5)
present ??= compute()            // ??= is  present = present ?? compute()
echo present                     // 5  (compute() was never called)
```

## `panic` and `assert`

For genuinely unrecoverable states:

- `panic(msg)` aborts the program (recorded as E0010) with a nonzero exit; output produced before it is kept.
- `assert(cond)` / `assert(cond, msg)` checks a condition and panics if it is false. It is the basis of the [test runner](Testing); the message is materialized only on failure.

```lang
assert(balance >= 0, "balance went negative")
```

## Choosing between them

| Situation | Use |
|---|---|
| A value might be missing | `?T` (`some`/`none`) |
| An operation might fail, and the caller should decide | `Result<T, E>` |
| Thread a failure up several call frames | `?` |
| Supply a default for a missing value | `??` |
| A bug / impossible state | `panic` / `assert` |

## See also

- [The Type System](Type-System) — how `?T`, `Result`, and unions fit the type lattice.
- [Standard Library](Standard-Library) — the option-returning stdlib methods.
