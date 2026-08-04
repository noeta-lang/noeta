# The Type System

Noeta is **inferred-static**: types are checked at compile time, signatures are required at named boundaries, and bodies are inferred. `dyn` is the single explicit escape into dynamic typing. This page covers the surface — the type forms you write and the operations that move between them. You have met most of these forms on the preceding pages already; this page is the map that puts them in one place. For how the checker works internally, see [The Type Checker](Type-Checker-Internals).

## The shape of it

- **Annotations are checked, then erased.** They are mandatory on a named function's parameters and return, and on fields; almost everywhere else they are optional and inferred.
- **Inference is local.** Bindings and closures infer their types from their initializers and bodies; there is no whole-program type reconstruction.
- A program with type errors is **rejected before it runs** — the type checker is a shared front-end upstream of both execution backends.

```noeta
fn add(a: int, b: int): int { return a + b }   // signature required
xs = [1, 2, 3]                                  // inferred List<int>
sq = fn(n) => n * n                             // inferred (int) -> int
```

## Type forms

| Form | Meaning |
|---|---|
| `int` `float` `f32` `f64` `bool` `string` `bytes` `void` | Primitives. |
| `number` | Any numeric scalar — `int`, `float`, `f32`, `f64`, and every [fixed width](Fixed-Width-Integers). |
| `List<T>` `Map<K, V>` `Set<T>` | Collections. |
| `?T` | Optional (`Option<T>`). |
| `Result<T, E>` | Fallible result. |
| `(A) -> R` | Function type. |
| `(A, B)` | Tuple. |
| `A \| B` | Union (a *closed* dynamic). |
| `dyn` | The open top — any value. |
| `never` | The bottom — no value at all (see below). |
| `Struct` `Class` `Enum` | Abstract kind-types (see below). |

## `number` — any numeric scalar

`number` names the set every numeric type belongs to, so a function that genuinely accepts any of them says so in one word instead of a twelve-member union:

```noeta
fn scaled(x: number, by: number): number { return x }

// sample:start
echo scaled(2, 3)          // int
echo scaled(2.5, 3.0)      // float
echo scaled(2.0f32, 3i16)  // the strict fixed widths, mixed freely
// sample:end
```

It is a **union**, not a thirteenth scalar — `number` *is* `int | float | f32 | f64 | i8 | … | u64`, written short. So it behaves like any other union: a value of any member widens into it, `x is number` narrows out of a `dyn`, and the strict fixed-width rules are untouched (`number` does not make an `f32` and an `int` add together — each member keeps its own arithmetic).

Because it is a set rather than a storage class, it cannot be a `@packed` field or a map key: both need one concrete width to lay out or hash by.

## Optionals — `?T`

There is no `null`. Absence is the value `none`; presence is `some(x)`. See [Error Handling](Error-Handling) for `?`/`??` and the full story.

```noeta
fn head(xs: List<int>): ?int { return xs.first() }
echo head([]) ?? -1     // -1
```

An optional is **its own reified type**, not a union of `T` and absence — `some(x)` really is a wrapper, and `echo` shows it as one. So `x is T` on a `?T` is *always false*: the value's runtime head constructor is `some`/`none`, never the payload's. The checker says so (**E0065**, a warning) and declines to narrow on such a test, so the unreachable branch does not go on type-checking as the payload. `Result<T, E>` behaves identically (`Ok`/`Err` are its head constructors).

```noeta check
struct P { x: int }

fn f(p: ?P): int {
    // `p is P` would be E0065 — always false. Take the option apart instead:
    return match p {
        some(v) => v.x,
        none    => 0,
    }
}
echo f(some(P { x: 7 }))    // 7
```

For mere presence, compare against the value: `p != none`. And `p is none` / `p is some` name *constructors*, not types — they are E0013, with the working spelling in the help.

## Unions — `A | B`

A union is a **closed** dynamic: a value is *one of* a known, finite set of types.

```noeta check
fn parse(s: string): int | string {
    // the number, or the original string on failure
    return match s.to_int() {
        some(n) => n,
        none    => s,
    }
}
```

- A member value **widens in** automatically: `int <: int | string`. A non-member is E0007.
- A union is exhaustively matchable with **no `_`** — one `is T` arm per member (see [Control Flow & Pattern Matching](Control-Flow-and-Pattern-Matching)).
- Narrow back to a member with `.as<T>()` or a match arm.
- `?A | B` parses as `(?A) | B` (the `?` binds tighter than `|`).

## `dyn` — the open top

`dyn` is the escape hatch: any value fits, and nothing is known statically. Unlike a union, `dyn` is *open* — no finite set of `is T` arms can exhaust it, so a `match` over `dyn` requires a `_` arm (E0011 without one).

```noeta
d: dyn = 42
echo d is int          // true
echo d is string       // false
```

## `never` — the bottom

`never` is `dyn`'s exact opposite. Where every type widens *into* `dyn`, `never` widens into *every* type; where `dyn` is inhabited by every value, `never` is inhabited by none. It is the return type of a function that **does not return**:

```noeta check
use std.os
use std.http.server

os.exit(1)                 // `os.exit(code?): never`
server.serve(8080, fetch)  // `server.serve(port, handler, host?): never` — the accept loop
panic("unreachable")       // `panic(msg): never`
```

Because `never` is a subtype of everything, a call to one of these type-checks in any position — there is no value to be wrong about — and nothing written after it can run.

You write it on your own functions the same way:

```noeta check
fn die(msg: string): never { panic(msg) }

// `never <: T`, so the same call satisfies an `int` return with nothing after it.
fn width(kind: string): int {
    if kind == "known" { return 4 }
    die("no width for ${kind}")
}
```

It is **declared, never inferred**. A function returns `never` because its signature says so, not because the checker proved its body loops forever — so divergence is a fact you can read off a signature, at the call site, without interprocedural analysis. That is what makes it useful to tooling: `noeta test` reads it to decide which top-level statements belong in the setup every test shares, which is why a top-level `server.serve(…)` never blocks a test run (see [Testing](Testing#what-runs-and-what-does-not)).

Two consequences follow from being uninhabited. `x is never` is always `false` — no value has the type. And `never` vanishes from a union: `int | never` *is* `int`, because the second arm contributes no values.

## Type tests and narrowing

**`x is T`** is a plain `bool` head-constructor test — well-formed even on a concrete `x`. Generics don't affect dispatch (one compiled shape serves every instantiation), but values carry a reified type tag, so `x is List<int>` is element-precise — it really does test the element type, not just "is `x` a list."

```noeta
enum Color { Red; Green }
d: dyn = Color.Green
echo d is Enum          // true
```

**`.as<T>()`** is a *checked narrowing* of a `dyn` or union to `?T` — `some(x)` if the runtime head constructor is `T`, else `none`. Narrowing an already-concrete (non-dynamic) value is E0028.

```noeta
struct Point { x: int  y: int }

fn as_point(x: dyn): ?Point { return x.as<Point>() }

fn kind(x: int | string): string {
    if x.as<int>() != none { return "int" }
    return "string"
}
echo kind(5)            // int
```

An `is` test also **flow-narrows**: inside `if x is T { … }` the checker sees `x` as `T`.

**These two are the answer whenever you can name the candidates**, and they beat reflection at it — they are checked at compile time and they narrow, which `type_of` does neither of. Reach for `type_of` and the `Type` ADT only when there *is* no candidate set to enumerate. See [Choosing a reflection surface](Attributes-and-Reflection#choosing-a-surface).

### Trait-object tests are precise membership

**`x is dyn Trait`** (and `.as<dyn Trait>()`) is a *precise membership test*: it is `true` iff the value's runtime nominal type has a **registered implementation** of the trait — a standalone `impl Trait for T`, an in-body `impl Trait { … }` block, a `@derive(Trait)`, or a native type's ABI-declared impl. The test is driven by the same registration data trait-method dispatch resolves through, so `x is dyn Trait` being `true` and "calling a `Trait` method on `x` works" can never disagree. Inside the `true` branch, `x` flow-narrows to `dyn Trait` and its trait methods dispatch — typed from the trait's declaration, `async` included (see [Trait objects](Generics-and-Traits#trait-objects)), so a narrowed receiver and a `dyn Trait` parameter type a call identically.

```noeta
trait Speaks { fn speak(): string }

struct Dog { name: string }
impl Speaks for Dog { pub fn speak(): string { return "woof" } }

struct Silent { name: string }

fn voice(x: dyn): string {
    if x is dyn Speaks { return x.speak() }
    return "..."
}
echo voice(Dog { name: "Rex" })     // woof
echo voice(Silent { name: "Sam" }) // ...
echo voice(42)                      // ...
```

Two edges worth knowing:

- **Non-nominal values never match.** Scalars, collections, and functions carry no nominal type, so they implement no *declared* trait. `42 is dyn Display` is `false` even though `echo 42` works — the built-in base types' protocol behavior is structural, not a registered `impl`, and only registered impls count. Use the head test (`x is int`) for built-ins.
- **`Self::Name` projections stay permissive.** A `.as<Self::Item>()`-style associated-type target has no runtime head to test (the binding is per-impl, and the erased value carries no impl identity), so it still matches any value — unlike the now-precise trait-object target.

`dyn Trait` is a *declared* bound, never a value's own type, and the two reflection queries split on exactly that line: `type_of(x)` on a value held behind a `dyn Trait` binding reports the **concrete** type (`Type.Struct(Dog, [])`), because that is what the value is, while `params_of` on a `fn f(x: dyn Speaks)` reports `Type.DynTrait(Speaks)`, because that is what the signature says. Neither is the other's answer, and a framework injecting a service by its interface needs both.

## Abstract kind-types

`Struct`, `Class`, and `Enum` are supertypes of every declared type of that kind — useful for runtime kind tests against a `dyn`:

```noeta
enum Color { Red; Green }
d: dyn = Color.Green
echo d is Enum              // true
echo d is Struct            // false (it's an enum, not a struct)
```

## Where inference stops

A value that the checker cannot pin down is an error at the boundary, not a silent `dyn`:

- An immutable, unannotated binding to a context-free literal (`[]`, `{}`, an ambiguous `Ok(x)`) is E0023 — annotate it or accumulate into a `mut`.
- A missing parameter or return type on a named function is E0022.

This is the "inferred-static" contract: no holes at named boundaries, inference in the interior.

## Stable bindings and explicit numeric conversion

Two rules keep a binding's static type **trustworthy**: a `mut` binding's type is fixed at declaration (declare a union or `dyn` for a binding that must hold more), and numeric conversion is explicit at a boundary (`int` is not a subtype of `float`, though the two still promote inside an arithmetic expression). Both are covered in full in [Syntax Basics](Syntax-Basics#bindings-and-mutability) — see also its [numeric-conversion note](Syntax-Basics#number-literals).

## See also

- [Error Handling](Error-Handling) — `Option`, `Result`, `?`, `??`.
- [Generics & Traits](Generics-and-Traits) — type parameters and bounds.
- [Attributes & Reflection](Attributes-and-Reflection) — `type_of`, the runtime `Type` value, and [when to use it instead of `is`](Attributes-and-Reflection#choosing-a-surface).
- [The Type Checker](Type-Checker-Internals) — the bidirectional engine behind all of this.
