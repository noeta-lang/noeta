# Generics & Traits

Generic types and functions, the built-in trait set that operators dispatch through, and `@derive`.

## Generics

Type parameters `<T>`, `<A, B>` go on functions, structs, classes, and enums. They are **erased at runtime** (one shape serves every instantiation).

```noeta
class Box<T> {
    pub value: T
    fn new(v: T): Box<T> { return Box { value: v } }
    fn get(): T { return value }
}

struct Pair<A, B> { first: A  second: B }

enum Opt<T> { None; Some(value: T) }
```

The element type is **tracked through instances and literals**: `Box.new(42)` is a `Box<int>`, and `Box { value: "hi" }` is a `Box<string>`. A mismatch downstream is E0007.

## Bounds

Constrain a parameter to a built-in trait with `<T: Trait>`:

```noeta
fn max<T: Comparable>(a: T, b: T): T {
    if a > b { return a }
    return b
}
echo max(3, 9)          // 9
echo max("a", "b")      // b
```

Bounds are enforced statically at both ends:

- **Body-side requirement** — using `>` requires `Comparable`, `+` requires `Add`; on an *unbounded* `T` that operation is E0025 at the definition.
- **Call-site check** — instantiating with a type that does not satisfy the bound is E0025. The first argument pins `T`; later arguments are checked against that substitution (E0007 on mismatch).
- An unknown bound name is E0014.

## The built-in traits

Traits are a **fixed built-in set** — naming an unknown one is E0014. Operators dispatch to a trait's method:

| Trait | Method | Lights up |
|---|---|---|
| `Equatable` | `eq(other): bool` | `==` `!=` |
| `Comparable` | `compare(other): Ordering` | `< <= > >=` |
| `Display` | `to_string(): string` | `echo`, `${…}` |
| `Add` | `add(other): T` | `+` |
| `TryAdd` | `try_add(other): Result<T, E>` | fallible `+` (via `?`) |
| `Index` | `get(i): T` | `a[i]` |
| `Length` | `len(): int` | `x.len()` on a `<T: Length>` parameter |
| `Iterable` | `iter(): Iterator<T>` | `for x in o` |
| `Clone` | — | structural clone |

`Ordering` is a namable built-in enum (`Ordering.Less` / `Equal` / `Greater`); calling `.compare()` on a primitive returns it.

## Implementing a trait

Implement a trait **in the type's body** with `impl Trait { }` (uniform across class, struct, and enum):

```noeta
class Money {
    amount: int
    fn new(a: int): Money { return Money { amount: a } }
    impl Add {
        fn add(other: Money): Money { return Money { amount: amount + other.amount } }
    }
    impl Comparable {
        fn compare(other: Money): Ordering { return amount.compare(other.amount) }
    }
}
echo (Money.new(3) < Money.new(5))   // true
```

There is also a **standalone** `impl Trait for T { }` (marker/empty-body only for now), which must target a type declared in the same module — an orphan target is E0013, a wrong or missing method is E0015.

## `@derive` — synthesized implementations

`@derive(...)` generates trait impls from a type's shape. It is a *codegen* directive, distinct from `#[...]` data attributes (see [Attributes & Reflection](Attributes-and-Reflection)).

| Derivable | Effect |
|---|---|
| `Equatable` | Structural equality. |
| `Comparable` | Field-wise ordering, in declaration order (recurses into nested objects). |
| `Display` | A structural `to_string`. |
| `Clone` | A structural clone. |
| `Serialize<Json>` | Synthesizes `to_json()`. |

```noeta ignore
@derive(Equatable, Comparable, Display, Clone)
class Point {
    x: int
    y: int
    fn new(x: int, y: int): Point { return Point { x: x, y: y } }
}
echo Point.new(1, 2) < Point.new(1, 3)   // true

@derive(Serialize<Json>)
class User { name: string  id: int  active: bool }
echo User.new("Ada", 7, true).to_json()  // {"name":"Ada","id":7,"active":true}
```

Errors: deriving a non-derivable trait (`@derive(Add)`) or wrong generic arity (`@derive(Comparable<int>)`, `@derive(Serialize)` without a format) is E0014. The old `#[derive(...)]` spelling is E0017.

## Coherence

Each type has **at most one** implementation of a given trait, across `@derive(T)`, an in-body `impl T { }`, and a standalone `impl T for Type { }`. A duplicate or competing impl is E0027. Because all traits are built-in and `impl` blocks may only target a type declared in the same module, the orphan problem is structurally impossible — coherence checking reduces to uniqueness.

## Operator errors

Applying an operator to a type that does not implement its trait is an error: `+` or `<` on a plain struct is E0007; `===` on a value type (a struct) is E0034 (identity is a class-only concept).
