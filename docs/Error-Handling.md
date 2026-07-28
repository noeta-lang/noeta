# Error Handling

No `null`, no exceptions. Absence and failure are ordinary values you pass, match, and propagate.

## `Option` — `?T`

An optional value is either `some(x)` or `none`. The type is written `?T`.

```noeta
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

```noeta check
struct Item { price: float }

enum OrderError { Empty; NegativePrice(index: int) }

fn validate(items: List<Item>): Result<void, OrderError> {
    if items.len() == 0 { return Err(OrderError.Empty) }
    return Ok()
}

echo match validate([]) {
    Ok()   => "valid",
    Err(e) => "invalid: ${e}",
}
```

## The `Error` trait

A type whose values describe a failure implements the built-in `Error` trait — one required method, `message(): string`. An `Error`-implementing value is the idiomatic `Err` payload: the caller can always ask *what went wrong* without knowing the concrete error type.

```noeta
struct ParseFailure {
    at: int

    impl Error {
        fn message(): string { return "bad digit at ${self.at}" }
    }
}

fn digit(c: string): Result<int, ParseFailure> {
    return Err(ParseFailure { at: 3 })
}

echo match digit("x") {
    Ok(n)  => "ok: ${n}",
    Err(e) => "failed: ${e.message()}",
}
```

`Error` is independent of `Display`: implementing it never changes how the value renders (an `Err(e)` echoes with the payload's ordinary display), and an error type may *also* implement `Display` when its message is the natural rendering. `<E: Error>` works as a generic bound, so helpers can be polymorphic over any error type. The standard library's first implementor is [`JsonError`](std-json), the payload of `json.try_parse` (the dynamic door), `json.try_parse::<T>`, and `json.decode_typed`.

> [!TIP]
> **Reading a document whose shape is not yours.** A body off a wire carries the *remote party's* shape, so a malformed one is bad input rather than a bug in your program — it must be a value you handle. `json.try_parse(text)` is that door: it needs no declared type, returns `Result<dyn, JsonError>`, and carries the same `path()`/`kind()`/`line()`/`column()` detail the typed doors do. `json.parse(text)` stays the aborting spelling, for when a malformed document really does mean the program is wrong (E0007).

```noeta
use std.json

echo match json.try_parse("{ nope") {
    Ok(doc) => "read ${doc["id"]}",
    Err(e)  => "bad payload (${e.kind()}) at line ${e.line() ?? 0}: ${e.message()}",
}
```

### Deriving `Error`

`@derive(Error)` synthesizes `message()` as `"${self}"` — the failure description **is** the type's display story, so `message()` and how the value renders can never disagree. With an `impl Display`, that is your hand-written `to_string()`; with `@derive(Display)`, the structural rendering you opted into. A type with neither cannot derive `Error` (E0050): the message would be an accidental structural dump.

```noeta
@derive(Error)
struct HaltError {
    code: int

    impl Display {
        fn to_string(): string { return "halted with code ${self.code}" }
    }
}

echo HaltError { code: 3 }.message()      // halted with code 3
```

For a **wrapper** error holding an inner failure, delegate instead: `@derive(Error, via: field)` forwards `message()` into the field's own `Error` implementation (the field's type must implement `Error`, hand-written or itself derived — E0050 otherwise).

```noeta check
struct ParseFailure {
    at: int
    impl Error {
        fn message(): string { return "bad digit at ${self.at}" }
    }
}

@derive(Error, via: cause)
struct ConfigError {
    path: string
    cause: ParseFailure
}
```

## `?` — propagate a failure

On a `Result` or `Option`, the postfix `?` unwraps the success value, or **early-returns** the `Err`/`none` from the enclosing function. Using `?` on any other type is E0012, and so is using it where the early return has nowhere to go — see [The enclosing function has to be able to return what `?` returns](#the-enclosing-function-has-to-be-able-to-return-what--returns). When the propagated `Err`'s type differs from the function's declared error type, `?` converts it through a declared `From` conversion — see [Converting errors at `?`](#converting-errors-at---impl-fromsource).

```noeta ignore
use std.id.{next_id}
fn place(items: List<Item>): Result<Order, OrderError> {
    validate(items)?                        // returns the Err here if invalid
    return Ok(Order.new(next_id(), items))
}
```

This lets you write the happy path linearly while failures short-circuit outward:

```noeta ignore
fn pipeline(path: string): Result<Report, Error> {
    raw    = fs_read(path)?      // returns Err on read failure
    parsed = parse(raw)?         // returns Err on parse failure
    return Ok(analyze(parsed))
}
```

`?` works the same on an **Option**: it unwraps a `some`, or early-returns the `none` from the enclosing (Option-returning) function:

```noeta
fn head(xs: List<int>): ?int { return xs.first() }

fn first_doubled(xs: List<int>): ?int {
    n = head(xs)?          // a `none` short-circuits out of the function here
    return some(n * 2)
}

echo first_doubled([3, 4])   // some(6)
echo first_doubled([])       // none
```

### The enclosing function has to be able to return what `?` returns

`?` is an early return, so the signature must carry what it returns — one rule, applied to whichever half you used:

| `?` on | Needs a return of | Otherwise |
|---|---|---|
| an `Option` (it early-returns `none`) | `?T` | **E0012** |
| a `Result` (it early-returns the `Err`) | `Result<T, E>` | **E0012** |

```noeta error
fn head(xs: List<string>): string {
    return xs.first()?   // E0012: `?` on an `Option` early-returns `none`, but this returns `string`
}
echo head([])
```

```noeta error
use std.http.client
fn fetch(url: string): void {
    r = client.get(url)?   // E0012: `?` on a `Result` early-returns its `Err`, but this returns `void`
    echo r.status()
}
fetch("https://example.com")
```

The fix is whichever of these fits the caller you have in mind:

- **Declare the return.** `?T` for the absence, `Result<T, E>` for the failure — and if the propagated `Err` type differs from `E`, [convert it through `From`](#converting-errors-at---impl-fromsource).
- **Handle it here.** `match` the `Option`/`Result` and answer with a value of your own, or supply a fallback with [`??`](#--coalesce).

A return type that **defers** — `dyn`, an unannotated closure, or top-level code, which declares no return at all — accepts `?` without a diagnostic, as everywhere in the gradual checker. The judgement then lands at runtime instead of being discarded:

- An `Err` that propagates all the way **out of the top level** aborts the program with the error's `message()` and a **non-zero exit** (**E0069**). Output produced before it is kept and nothing after it runs, exactly as [`panic`](#panic-and-assert) behaves. So a top-level `?` is a legitimate shape for a script's entry point — "do the work, and let a failure stop the program loudly" — and a broken run can never be mistaken for a clean one.
- A `none` reaching the top is not a failure and ends the program normally.

```noeta ignore
use std.http.client
// No declared return at the top level, so `?` is accepted here. A transport failure aborts with
// E0069 and the error's message rather than exiting 0 in silence.
r = client.get("https://api.example.com/health")?
echo r.status()
```

## Converting errors at `?` — `impl From<Source>`

A pipeline usually crosses error types: `json.try_parse` fails with a `JsonError`, your function returns `Result<T, AppError>`. A `?` whose `Err` payload type differs from the enclosing function's declared error type **converts** it through a declared conversion — the built-in `From` trait, implemented **on the target** error type:

```noeta
use std.json
use std.json.JsonError

struct User { name: string  age: int }

struct AppError {
    detail: string

    impl From<JsonError> {
        fn from(e: JsonError): AppError {
            return AppError { detail: "decode failed: ${e.message()}" }
        }
    }
}

fn load(text: string): Result<string, AppError> {
    u = json.try_parse::<User>(text)?     // Err(JsonError) → Err(AppError.from(e))
    return Ok(u.name)
}

echo load("{ nope")
```

The rules keep the language explicit:

- **`?` is the only implicit conversion position.** A `return Err(jsonErr)` or an assignment with a mismatched error type stays the plain type mismatch (E0007) it always was; write `Err(AppError.from(e))` there — `from` is an ordinary associated function, callable anywhere as `Target.from(x)`.
- **Exactly one conversion path.** The conversion is declared on the target (`impl From<Source>` names the source; the source may be an extern type like `JsonError`, which could not carry your impl anyway — an `impl` targets a struct, class, or enum the program [declares](Generics-and-Traits#implementing-a-trait), never a built-in or an extern). A type carries at most one `From` impl — a second, whatever its source, is a coherence conflict (E0027) — and conversions never chain. Declaring it on the target is also what satisfies the [orphan rule](Generics-and-Traits#the-orphan-rule): `From` is built into the language and so belongs to no package, which means the `impl` must live in the target type's own package. A conversion *into* a dependency's error type therefore goes on your own wrapper, never on theirs.
- **No conversion, no propagation.** A `?` whose `Err` type neither matches the declared error type nor has a `From` conversion is E0057. (A `dyn`/unannotated context defers to runtime, as everywhere in the gradual checker.)
- `from` is an **associated** conversion: it builds a new target value from its argument, so a body referencing `self` is rejected (E0015), as is a parameter that disagrees with the declared source.

## `??` — coalesce

`expr ?? fallback` unwraps a `some`, or evaluates the fallback for a `none`/absent value. It short-circuits — the fallback runs only when needed:

```noeta
fn find(hit: bool): ?string {
    if hit { return some("ada") }
    return none
}
fn compute(): ?int { return some(9) }

echo find(false) ?? "guest"      // guest  (find returned none)

mut present = some(5)
present ??= compute()            // ??= is  present = present ?? compute()
echo present                     // 5  (compute() was never called)
```

## `panic` and `assert`

For genuinely unrecoverable states:

- `panic(msg)` aborts the program (recorded as E0010) with a nonzero exit; output produced before it is kept.
- `assert(cond)` / `assert(cond, msg)` checks a condition and panics if it is false. It is the basis of the [test runner](Testing); the message is materialized only on failure.
- An unhandled `Err` reaching the top level is the *third* way a program aborts (E0069, above) — the difference is that nobody wrote it: it is where an ordinary recoverable failure ends up when no frame handled it.

```noeta
balance = 5
assert(balance >= 0, "balance went negative")
```

## Choosing between them

| Situation | Use |
|---|---|
| A value might be missing | `?T` (`some`/`none`) |
| An operation might fail, and the caller should decide | `Result<T, E>` |
| Thread a failure up several call frames | `?` (the frames in between declare `Result<T, E>`) |
| Let a failure stop a script, loudly | `?` at the top level (aborts with the error's message, non-zero exit) |
| Supply a default for a missing value | `??` |
| A bug / impossible state | `panic` / `assert` |

## See also

- [The Type System](Type-System) — how `?T`, `Result`, and unions fit the type lattice.
- [Standard Library](Standard-Library) — the option-returning stdlib methods.
