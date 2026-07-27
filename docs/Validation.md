# Validation

Noeta enforces **data-boundary invariants** with one small vocabulary: the `Validate` trait, its
automatic enforcement at every typed-decode **door** — an entry point like `json.parse::<T>` or
`from_bytes::<T>` that materializes typed values from raw input (see [Native
Extensions](Native-Extensions) for the machinery) — and the `@validated` construction marker.
Together they let a type guarantee that no value of it is ever ill-formed — whether it is built in
code or decoded from an untrusted document.

## The `Validate` trait

A type implements `Validate` by providing one method, `validate()`, which returns `Result<void, E>`:
`Ok()` when the value is well-formed, `Err(e)` when an invariant is violated. The error `E` is either
a plain `string` or any type that implements [`Error`](Generics-and-Traits#the-built-in-traits) — the
principled form, since a validator's error then converts at `?` like any other error.

```noeta
struct Port {
    n: int

    impl Validate {
        fn validate(): Result<void, string> {
            if self.n < 1 || self.n > 65535 {
                return Err("port out of range: ${self.n}")
            }
            return Ok()
        }
    }
}

// `.validate()` dispatches like any instance method.
echo match (Port { n: 8080 }).validate() {
    Ok() => "ok",
    Err(msg) => msg,
}
echo match (Port { n: 70000 }).validate() {
    Ok() => "ok",
    Err(msg) => msg,
}
```

`Validate` is **not derivable** — an invariant cannot be synthesized from a type's fields, so there is
nothing for `@derive` to generate; you always write the one method. It is independent of `Display`:
adopting `Validate` never changes how a value renders. Because `validate()` returns a `Result`, it
composes with `?` — `x.validate()?` short-circuits like any fallible call, converting its error
through `From` when the enclosing function's error type differs.

Prefer `Validate` over ad-hoc checks in a constructor whenever the type can also arrive through a
decode door: the invariant is then written once and guards both paths — construction in code and
untrusted data at the boundary — instead of living only in the constructor a decoder never calls.

## Automatic enforcement at decode

The point of `Validate` is that it runs **automatically** wherever untrusted data crosses into a typed
value. When a decode door materializes a value whose type implements `Validate`, its `validate()` runs
on the freshly-built value — you never call it by hand at the boundary.

Enforcement is **bottom-up**: a type's fields are decoded and validated before the type's own
`validate()` runs, so a container only ever validates already-valid fields, and a nested failure
points at the innermost value. A JSON failure is a path-carrying [`JsonError`](std-json)
reading `field[i]: <message>`.

```noeta ignore
use std.json
use std.json.JsonError

// … the same `Port` (with its `impl Validate`) as above …

struct Cluster {
    ports: List<Port>
}

fn describe(r: Result<Cluster, JsonError>): string {
    return match r {
        Ok(c) => "ok: ${c.ports.len()} ports",
        Err(e) => e.message(),
    }
}

// The second port is out of range — the error names it exactly.
echo describe(json.try_parse::<Cluster>("{\"ports\": [{\"n\": 80}, {\"n\": 70000}]}"))
// ports[1]: port out of range: 70000
// A well-formed document passes untouched.
echo describe(json.try_parse::<Cluster>("{\"ports\": [{\"n\": 443}]}"))
// ok: 1 ports
```

Each door decides how a rejection surfaces — the same choice it already makes for a shape mismatch:

| Door | On a validation failure |
|---|---|
| `json.parse::<T>(text)` | **Aborts** at the call site (runtime `E0007`, the general type-mismatch code), message `json.parse: <path>: <msg>`. |
| `json.try_parse::<T>(text)` | **Recoverable**: `Result.Err(JsonError)` with the path-carrying message. |
| `json.decode_typed(name, text)` | **Recoverable**: `Result.Err(JsonError)` (the router-facing decode). |
| `from_bytes::<T>(bytes)` | **Aborts** at `[i]` (runtime `E0007`) — a `@packed` type may `impl Validate`, and each decoded element is checked. |

A type with no validator pays nothing: the decode walk never re-enters for it.

## `@validated` — channeling construction

Automatic decode-time validation guards the data boundary. To also guarantee that *code* cannot build
an ill-formed value, mark the type `@validated`. Then literal construction (`T { ... }`, and the
record-update spread `T { ...base, f: v }`) from **outside** the type's own `impl`/methods is a compile
error (`E0060`, the code for a literal that bypasses a `@validated` type's constructors) — a value
can only be built through a constructor the type provides, which runs `validate()` and returns a
`Result`.

```noeta
@validated
struct Email {
    addr: string

    // The sanctioned constructor. Inside the type's own methods, literal construction stays legal.
    fn new(addr: string): Result<Email, string> {
        e = Email { addr: addr }
        e.validate()?
        return Ok(e)
    }

    impl Validate {
        fn validate(): Result<void, string> {
            if !self.addr.contains("@") {
                return Err("missing @: ${self.addr}")
            }
            return Ok()
        }
    }
}

// sample:start
echo match Email.new("a@b.com") {
    Ok(e) => "ok: ${e.addr}",
    Err(m) => m,
}
echo match Email.new("nope") {
    Ok(e) => "ok: ${e.addr}",
    Err(m) => m,
}
// sample:end
```

Building a `@validated` type with a bare literal from outside its `impl` is rejected:

```noeta error
@validated
struct Email {
    addr: string

    impl Validate {
        fn validate(): Result<void, string> { return Ok() }
    }
}
// E0060: `Email` is `@validated` — build it through a constructor, not a literal.
e = Email { addr: "x@y" }
echo e.addr
```

`@validated` is **purely static** — it changes no runtime behavior and no expression's type. It
composes with [field privacy](Structs-Classes-and-Enums): both checks can fire on the same literal.
And it does **not** restrict the recipe doors: a `@validated` type decoded from JSON or `from_bytes`
is built directly and its `validate()` runs automatically — that is exactly the guarantee `@validated`
completes, so the boundary and the constructor are the only two ways in, and both validate.
