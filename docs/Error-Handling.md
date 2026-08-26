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
        pub fn message(): string { return "bad digit at ${self.at}" }
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
        pub fn to_string(): string { return "halted with code ${self.code}" }
    }
}

echo HaltError { code: 3 }.message()      // halted with code 3
```

For a **wrapper** error holding an inner failure, delegate instead: `@derive(Error, via: field)` forwards `message()` into the field's own `Error` implementation (the field's type must implement `Error`, hand-written or itself derived — E0050 otherwise).

```noeta check
struct ParseFailure {
    at: int
    impl Error {
        pub fn message(): string { return "bad digit at ${self.at}" }
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
        pub fn from(e: JsonError): AppError {
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

A pipeline usually crosses more than one, so a target declares a conversion from **each** source it absorbs — one `impl From<Source>` block per source, and each `?` converts through the one its own error type names:

```noeta
struct HttpError { status: int }
struct JsonError { line: int }

struct AppError {
    detail: string

    impl From<HttpError> {
        pub fn from(e: HttpError): AppError {
            return AppError { detail: "request failed: ${e.status}" }
        }
    }

    impl From<JsonError> {
        pub fn from(e: JsonError): AppError {
            return AppError { detail: "bad body at line ${e.line}" }
        }
    }
}

fn fetch(id: string): Result<string, HttpError> { return Err(HttpError { status: 503 }) }
fn decode(body: string): Result<string, JsonError> { return Err(JsonError { line: 1 }) }

fn load(id: string): Result<string, AppError> {
    body = fetch(id)?          // HttpError → AppError
    return Ok(decode(body)?)   // JsonError → AppError
}

echo load("7")
```

The rules keep the language explicit:

- **`?` is the only implicit conversion position.** A `return Err(jsonErr)` or an assignment with a mismatched error type stays the plain type mismatch (E0007) it always was; write `Err(AppError.from(e))` there — `from` is an ordinary static function, callable anywhere as `Target.from(x)`.
- **One conversion per source.** The conversion is declared on the target (`impl From<Source>` names the source; the source may be an extern type like `JsonError`, which could not carry your impl anyway — an `impl` targets a struct, class, or enum the program [declares](Generics-and-Traits#implementing-a-trait), never a built-in or an extern). A type may declare a conversion from each of several sources — that is how one application error type absorbs a transport failure and a decode failure — but only one from each: a repeated source is a coherence conflict (E0027). Declaring it on the target is also what satisfies the [orphan rule](Generics-and-Traits#the-orphan-rule): `From` is built into the language and so belongs to no package, which means the `impl` must live in the target type's own package. A conversion *into* a dependency's error type is therefore written the other way round, with [`impl To<Target>`](#converting-into-a-type-you-do-not-own) on your own type.
- **Exactly one conversion path.** Which conversion runs is decided where the call is written, never at run time: a `?` converts through the one its propagated `Err` type names, and `Target.from(x)` through the one `x`'s type names. Sources are matched by type identity, so a site sees exactly one candidate — and **conversions never chain**. `A → B` and `B → C` do not make `A → C`; write the conversion you want.
- **No conversion, no propagation.** A `?` whose `Err` type neither matches the declared error type nor has a `From` conversion is E0057. (A `dyn`/unannotated context defers to runtime, as everywhere in the gradual checker.)
- `from` is an **associated** conversion: it builds a new target value from its argument, so a body referencing `self` is rejected (E0015), as is a parameter that disagrees with the declared source.
- **A conversion produces its target, and cannot fail.** Its signature is pinned at both ends — the declared source against the parameter, the target against the return — because `?` applies a conversion implicitly and propagates whatever it returns. A `from` returning `Result<Target, E>` would make the propagated `Err` hold a `Result`, disagreeing with the error type the function declares, so it is E0015. A conversion that *can* fail is an ordinary function returning a `Result`, called and propagated like any other.
- **A conversion is never guessed.** Where a target declares several, `Target.from(x)` needs `x`'s type to name one of them. An argument typed `dyn` leaves every conversion a candidate and is E0023 — narrow it (`if x is HttpError { … }`) or annotate it; an argument whose type names no declared source is E0007, and the diagnostic lists what the target does convert.
- **The conversion's own name carries its source.** A declared conversion answers to `from<HttpError>` rather than to `from`, which is what lets an enum keep its [wire-value conversion](Structs-Classes-and-Enums#converting-a-wire-value-to-a-case) — `Plan.from("free")` — while also declaring `impl From<Raw>`. Writing the call never needs the built name — `Target.from(x)` and `?` both pick by type — but reaching one by name does; see [A declared conversion is named after its counterpart](Attributes-and-Reflection#a-declared-conversion-is-named-after-its-counterpart).

## Converting *into* a type you do not own — `impl To<Target>`

A conversion's `impl` lives with its own type, so `impl From<Source>` needs the **target** to be yours. When the target belongs to somebody else — a framework's error type your handler has to return — there is nowhere to put it, and `?` has nothing to convert through.

`impl To<Target> for Source` states the same conversion from the other end, and lives with the source:

```noeta
// `ServiceError` stands in for a type from the framework you are writing against — the reason the
// conversion cannot go on it is that it is not yours to edit.
struct ServiceError { detail: string }
struct DiskError { path: string }

impl Error for ServiceError { pub fn message(): string { return self.detail } }
impl Error for DiskError {
    pub fn message(): string { return "cannot read ${self.path}" }
}

// `impl From<DiskError>` would have to sit inside `ServiceError`. This sits with `DiskError`.
impl To<ServiceError> for DiskError {
    pub fn to(): ServiceError { return ServiceError { detail: self.message() } }
}

fn read_config(): Result<string, DiskError> {
    return Err(DiskError { path: "/etc/app.toml" })
}

fn handler(): Result<string, ServiceError> {
    raw = read_config()?       // DiskError → ServiceError, through the `To` impl
    return Ok(raw)
}

echo match handler() {
    Ok(v)  => v,
    Err(e) => e.message(),
}
```

Everything the `From` rules say holds here, read from the other side:

- **A conversion is one relation, whichever spelling states it.** `impl From<A>` on `B` and `impl To<B>` for `A` both declare `A → B`, so a program containing both is a coherence conflict (E0027) naming both sites. That keeps "exactly one conversion path" true by construction rather than by a precedence rule.
- **The two can never collide across packages.** Each spelling needs its own type local and the other merely visible, so two packages declaring one conversion would have to depend on each other. A conflict is therefore always something one author can fix by deleting one of the two.
- **It follows that `To` cannot override a `From` you do not own.** Reaching a foreign target is what it is for; changing what an existing conversion means is not, and would let one package silently retarget a `?` in code that never mentions it.
- **One conversion per target.** A source may convert into several targets — one `impl To<Target>` block each, named apart by the target the way a target's conversions are named apart by their sources.
- **The call names the target.** `value.to::<Target>()` selects the conversion, because the bare `to` names a set rather than one of them. The type argument selects; it does not instantiate anything.
- `to` converts the value in hand, so it takes `self` and returns the declared target — a return type disagreeing with the trait argument is E0015, the mirror of the parameter check on `from`.

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

## Aborting and recoverable doors

Some standard-library operations come in **pairs**: an aborting door named for the operation, and a recoverable twin prefixed `try_` that returns a `Result` instead. The pair exists wherever the same call is a *bug* in one program and an *ordinary condition* in another, and the language refuses to guess which:

| Aborting | Recoverable | The condition |
|---|---|---|
| `json.parse(text)` | `json.try_parse(text)` | a malformed document |
| `os.spawn(cmd, args)` | `os.try_spawn(cmd, args)` | the program is not installed, or is not executable |
| `p.write(text)` | `p.try_write(text)` | the child's stdin is gone — it exited, or you closed it |
| `regex.compile(pattern)` | `regex.try_compile(pattern)` | the pattern is not a valid regular expression |

The rule for choosing is **whose mistake is it**. A configuration file your own build produced is your program's shape, so `json.parse` aborting on it is the right report; a body off a wire carries the *remote party's* shape, so `json.try_parse` is. A tool your own installer placed is yours; a language server, an MCP server, or a formatter the user may or may not have installed is not — and a library whose contract is "a failing tool is a turn, not an outage" cannot call the aborting door at all. A pattern you wrote into the source is yours, so `regex.compile` aborting on it is the right report too; a redaction or routing rule loaded from configuration is the operator's, and validating one before compiling it would mean shipping a second, worse regex engine.

What rides the `Err` side is whatever carries the failure's information. `JsonError` and `OsError` are types because a caller must *branch* on which failure it was — `e.kind()` distinguishes `"not_found"` from `"permission_denied"`, and the two want different handling. `regex.try_compile` answers a plain `string`: a pattern has exactly one way to be invalid, so a `kind()` there would be a constant, and the engine's own caret-carrying diagnostic is the whole value.

```noeta
use std.{regex}

rule = '(user-\d+'                  // as if read from configuration

echo match regex.try_compile(rule) {
    Ok(p) => "redacting ${p.source()}",
    Err(e) => "ignoring an unusable rule",
}
```

Both halves of a pair report the **identical message** — the aborting one is derived from the recoverable one — so moving between them never changes what the user sees, only who decides what happens next.

```noeta
use std.{os}

echo match os.try_spawn("mcp-server-filesystem", ["/tmp"]) {
    Ok(p)  => "started pid ${p.pid()}",
    Err(e) => match e.kind() {
        "not_found" => "install mcp-server-filesystem to use this tool",
        _           => "could not start it: ${e.message()}",
    },
}
```

> [!IMPORTANT]
> **A liveness check is not a substitute for the recoverable door.** Polling a child with `try_wait()` before writing to it looks like it makes `p.write(…)` safe, and it does not: the child can exit in the gap between the poll and the write. That race cannot be closed from inside the language, which is exactly why `try_write` exists. Branch on `e.kind()` — `"broken_pipe"` means the child is gone (restart it), `"stdin_closed"` means you closed the pipe yourself (a bug in your own code).

The aborting door is never *removed* when a recoverable twin is added. "Fatal by default, recoverable when you ask" is the shape, and a script that legitimately wants a missing program to stop the run should not have to write a `match` to get it.

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
| A failure that is a bug in one caller and routine in another | an [aborting/recoverable door pair](#aborting-and-recoverable-doors) (`parse` / `try_parse`) |
| A bug / impossible state | `panic` / `assert` |

## See also

- [The Type System](Type-System) — how `?T`, `Result`, and unions fit the type lattice.
- [Built-ins](Standard-Library) — the option-returning built-in methods.
