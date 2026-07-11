# The Type System

Noeta is **inferred-static**: types are checked at compile time, signatures are required at named boundaries, and bodies are inferred. `dyn` is the single explicit escape into dynamic typing. This page covers the surface — the type forms you write and the operations that move between them. For how the checker works internally, see [The Type Checker](Type-Checker-Internals).

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
| `int` `float` `f32` `f64` `bool` `string` `void` | Primitives. |
| `List<T>` `Map<K, V>` `Set<T>` | Collections. |
| `?T` | Optional (`Option<T>`). |
| `Result<T, E>` | Fallible result. |
| `(A) -> R` | Function type. |
| `(A, B)` | Tuple. |
| `A \| B` | Union (a *closed* dynamic). |
| `dyn` | The open top — any value. |
| `Struct` `Class` `Enum` `Record` | Abstract kind-types (see below). |

## Optionals — `?T`

There is no `null`. Absence is the value `none`; presence is `some(x)`. See [Error Handling](Error-Handling) for `?`/`??` and the full story.

```noeta
fn head(xs: List<int>): ?int { return xs.first() }
echo head([]) ?? -1     // -1
```

## Unions — `A | B`

A union is a **closed** dynamic: a value is *one of* a known, finite set of types.

```noeta ignore
fn parse(s: string): int | string {
    // returns the number, or the original string on failure
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

## Abstract kind-types

`Struct`, `Class`, `Enum`, and `Record` are supertypes of every declared type of that kind — useful for runtime kind tests against a `dyn`:

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

Two rules keep a binding's static type **trustworthy** — the type an editor shows for `mut x` is the type `x` actually has, from declaration onward:

- **A `mut` binding has a fixed type.** It is set at declaration (annotated, or inferred from the initializer) and does not drift: a reassignment must be assignable to it, else E0007; the type is never silently changed by a later write. Reassigning an *immutable* binding at all is E0006, caught statically. For a binding that legitimately holds more than one type, declare a **union** (`mut x: int | string`) or `dyn` — the same explicit choices you make anywhere else. (One inferred type is still *completed* by its first write: `mut acc = []` resolves its element type from the accumulator's later writes; see [E0023](#where-inference-stops).)

- **Numeric conversion is explicit at a boundary.** `int` is **not** a subtype of `float`: binding, passing, returning, or storing an `int` where a `float` is expected is E0007 — write `2.0`, not `2` (as Rust does, and unlike C/Java). Numbers still combine inside an **expression** — `int` and `float` promote in arithmetic (`x + 1` on a `float` `x` is a `float`) — and that result is then checked against its boundary like any other value, so a widened `float` can never reach an `int` binding implicitly.

## See also

- [Error Handling](Error-Handling) — `Option`, `Result`, `?`, `??`.
- [Generics & Traits](Generics-and-Traits) — type parameters and bounds.
- [Attributes & Reflection](Attributes-and-Reflection) — `type_of` and the runtime `Type` value.
- [The Type Checker](Type-Checker-Internals) — the bidirectional engine behind all of this.
