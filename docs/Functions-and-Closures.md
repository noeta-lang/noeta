# Functions & Closures

Named functions, default parameters, multiple return, closures, and the pipe operator.

## Named functions

`fn name(params): Ret { body }`. At this named boundary, **parameter types and the return type are mandatory** (a missing type is E0022). Bodies infer everything else.

```lang
fn add(a: int, b: int): int {
    return a + b
}

fn fib(n: int): int {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}
```

`return` yields a value. A `void` function may `return;` or simply fall off the end.

## Default (optional) parameters

A **trailing** parameter may have a default `name: T = expr`. A required parameter may not follow a defaulted one (E0026).

```lang
fn greet(name: string, greeting: string = "Hello"): string {
    return "${greeting}, ${name}!"
}
echo greet("Ada")         // Hello, Ada!
echo greet("Ada", "Hi")   // Hi, Ada!
```

> [!NOTE]
> A default is evaluated in **globals-only scope**. It may read module-level bindings but *not* other arguments, `self`, or fields — naming another parameter resolves to nothing at runtime (E0005). A default widens the accepted arity to a range.

## Multiple return via tuples

Return a [tuple](Structs-Classes-and-Enums#tuples) to hand back several values, and destructure at the call site:

```lang
fn divmod(a: int, b: int): (int, int) {
    return (a / b, a % b)
}
(q, r) = divmod(17, 5)   // q = 3, r = 2
```

## Closures

A closure is `fn(params) => expr` (arrow) or `fn(params) { … }` (block, with `return`). Unlike named functions, a closure's parameter and return types are **optional** — they are inferred:

```lang
add_base = fn(x) => x + base            // arrow; captures `base`
sumsq    = fn(xs) { mut t = 0; for x in xs { t = t + x * x }; return t }   // block body
classify = fn(n): string { if n > 0 { return "pos" }; return "nonpos" }    // return annotation
g        = fn(n: int, bump: int = 10) => n + bump                          // annotated + default
twice    = fn(f, x) => f(f(x))          // higher-order
```

Closures **capture their environment**, including `mut` cells that outlive the frame that created them:

```lang
fn make_counter(): () -> int {
    mut n = 0
    return fn() { n = n + 1; return n }   // captures and mutates `n`
}
c = make_counter()
echo c()   // 1
echo c()   // 2
```

## Function types

Function types are first-class surface syntax — write them in annotations and signatures:

```lang
apply: (int) -> int = fn(x) => x + 1
fn run(f: (int) -> int, x: int): int { return f(x) }
```

The prelude builtins (`len`, `sum`, `map`, `filter`) are themselves first-class values you can pass around.

## The pipe operator

`|>` threads the left value as the **first argument** of the right call. It turns nested calls into a left-to-right pipeline:

```lang
echo 5 |> inc |> inc          // inc(inc(5))
echo 5 |> add(10)             // add(5, 10)

echo [1, 2, 3, 4]
    |> filter(fn(n) => n % 2 == 0)
    |> map(fn(n) => n * 10)
    |> sum()                  // 60
```

## See also

- [Error Handling](Error-Handling) — `?` propagation and `Result`-returning functions.
- [Generics & Traits](Generics-and-Traits) — generic and bounded functions (`fn max<T: Comparable>`).
- [Concurrency](Concurrency) — `async fn`.
