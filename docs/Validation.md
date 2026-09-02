# Validation

Noeta enforces data-boundary invariants with one small vocabulary: the `Validate` trait, its automatic enforcement at every typed-decode **door**, and the `@validated` construction marker. A door is an entry point like `json.parse::<T>` or `from_bytes::<T>` that materializes typed values from raw input (see [Native Extensions](Native-Extensions) for the machinery).

## The `Validate` trait

A type implements `Validate` by providing one method, `validate()`, which returns `Result<void, E>`: `Ok()` when the value is well-formed, `Err(e)` when an invariant is violated. The error `E` is either a plain `string` or any type that implements [`Error`](Generics-and-Traits#the-built-in-traits), so that a validator's error converts at `?` like any other error. Any other return shape is E0015 at the method.

```noeta
struct Port {
    n: int

    impl Validate {
        pub fn validate(): Result<void, string> {
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

Three rules come with the trait:

- **`Validate` is not derivable** (E0014). An invariant does not follow from a type's fields, so `@derive` has no body to write and you always write the one method.
- It is independent of `Display`. Adopting `Validate` leaves rendering exactly as it was.
- `x.validate()?` short-circuits like any fallible call, converting its error through `From` when the enclosing function's error type differs.

Write the invariant as a `Validate` impl rather than as a check inside a constructor whenever the type can also arrive through a decode door. The one method then guards both paths, construction in code and untrusted data at the boundary.

## Automatic enforcement at decode

When a decode door materializes a value whose type implements `Validate`, its `validate()` runs on the freshly-built value. You never call it by hand at the boundary.

Enforcement is **bottom-up**: a type's fields are decoded and validated before the type's own `validate()` runs, so a container validates already-valid fields and a nested failure points at the innermost value. A JSON failure is a path-carrying [`JsonError`](std-json) reading `field[i]: <message>`.

The rule is about *types*, not only structs. An [enum-typed field](Derives#enum-typed-fields) decodes through the same door, so an enum implementing `Validate` has its `validate()` run on the freshly selected case exactly as a struct does. A wire value that names no case is refused by the decode itself, as a path-carrying `unknown_variant` error listing every accepted value, and reaches no validator.

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

// The second port is out of range, and the error names it exactly.
echo describe(json.try_parse::<Cluster>("{\"ports\": [{\"n\": 80}, {\"n\": 70000}]}"))
// ports[1]: port out of range: 70000
// A well-formed document passes untouched.
echo describe(json.try_parse::<Cluster>("{\"ports\": [{\"n\": 443}]}"))
// ok: 1 ports
```

Each door decides how a rejection surfaces, the same choice it already makes for a shape mismatch:

| Door | On a validation failure |
|---|---|
| `json.parse::<T>(text)` | **Aborts** at the call site (runtime `E0007`, the general type-mismatch code), message `json.parse: <path>: <msg>`. |
| `json.try_parse::<T>(text)` | **Recoverable**: `Result.Err(JsonError)` with the path-carrying message. |
| `json.decode_typed(name, text)` | **Recoverable**: `Result.Err(JsonError)` (the router-facing decode). |
| `from_bytes::<T>(bytes)` | **Aborts** at `[i]` (runtime `E0007`). A `@packed` type may `impl Validate`, and each decoded element is checked. |
| [`construct(name, fields)`](Attributes-and-Reflection#construct-enforces-impl-validate) | **Recoverable**: the door's own `Err(string)`, carrying the validator's message. |

A type with no validator pays nothing: the decode walk never re-enters for it.

**`construct` is a door too.** [`construct(name, fields)`](Attributes-and-Reflection#construct-enforces-impl-validate) is the reflective form of a `T { … }` literal, and what it consumes is data: a `List<dyn>` or a `Map<string, dyn>` off a CLI, a request body, a model's tool arguments. It therefore enforces `Validate` on the terms `json.try_parse` does, under the same condition, which is implementing `Validate` whether or not the type carries a decoration.

Its bottom-up order is structural rather than a property of the recipe walk. `construct` never builds a nested value, so every field value it is handed already passed its own door, and the defaulted slots are filled before the type's own `validate` runs.

## Channeling construction with `@validated`

Automatic decode-time validation guards the data boundary. `@validated` extends the guarantee to code: literal construction (`T { ... }`, and the record-update spread `T { ...base, f: v }`) from **outside** the type's own `impl` and methods is `E0060`, the code for a literal that bypasses a `@validated` type's constructors. A value can then only be built through a constructor the type provides, which runs `validate()` and returns a `Result`.

```noeta
@validated
struct Email {
    addr: string

    // The sanctioned constructor. Inside the type's own methods, literal construction stays legal.
    pub fn new(addr: string): Result<Email, string> {
        e = Email { addr: addr }
        e.validate()?
        return Ok(e)
    }

    impl Validate {
        pub fn validate(): Result<void, string> {
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
        pub fn validate(): Result<void, string> { return Ok() }
    }
}
// E0060: `Email` is `@validated` — build it through a constructor, not a literal.
e = Email { addr: "x@y" }
echo e.addr
```

`@validated` is **purely static**. It changes no runtime behavior and no expression's type, and it composes with [field privacy](Structs-Classes-and-Enums), where both checks can fire on the same literal.

The decode doors stay exempt from the literal ban, and they earn the exemption at runtime: a `@validated` type decoded from JSON or `from_bytes` is built directly and its `validate()` runs automatically. Reflective `construct` works the same way, so a rejection there arrives as the door's `Err` rather than as a compile error.
