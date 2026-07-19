# Functions & Closures

Named functions, default parameters, multiple return, closures, and the pipe operator.

## Named functions

`fn name(params): Ret { body }`. At this named boundary, **parameter types and the return type are mandatory** (a missing type is E0022). Bodies infer everything else.

```noeta
fn add(a: int, b: int): int {
    return a + b
}

fn fib(n: int): int {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}
```

`return` yields a value. A `void` function may `return;` or simply fall off the end.

## Sealed functions & the `use (…)` capture clause

A named function is **sealed**: its body sees its parameters and the program's *declarations*
(other functions, types, imports) — never the surrounding **value bindings**. A top-level `items`
does not leak into `fn place(items: …)`; inside the body, `items` means the parameter, always.
To read a surrounding binding, import it explicitly with a capture clause between the parameter
list and the return type:

```noeta
tax_rate = 0.25
fn with_tax(price: float) use (tax_rate): float {
    return price * (1.0 + tax_rate)
}
echo with_tax(100.0)   // 125.0
```

A capture is a **live view** of the named binding (a `mut` global mutated later is seen; writing
through the capture follows the binding's own `mut` rules). The clause works on methods and
nested `fn`s the same way — a nested `fn` importing an enclosing `mut` local is the explicit form
of a closure counter. Without the clause, a reference to a top-level binding is an error with the
fix spelled out (E0005: *add `use (name)` to the signature, or pass it as a parameter*), and a
bare assignment to an unlisted name simply declares a fresh local.

**No shadowing (E0059).** One name means one thing per scope stack. A binder — a closure
parameter, `for` variable, match-pattern binding, or fresh local — may not reuse a name already
bound in a scope it can see, and a binding may not reuse an imported name or module alias
(E0020). Sealing is what keeps this ergonomic: named-fn params conflict with nothing because the
surrounding bindings genuinely are not in scope. Where capture *is* implicit — anonymous
closures — the rule bites: `fn(base) => …` under a visible `base` is rejected; rename one.
(Reassignment is not shadowing — `x = 5` on an existing binding follows the `mut` rules E0006/E0007
— and `is`-narrowing refines the *same* binding, so neither ever needs a shadow.)

## Default (optional) parameters

A **trailing** parameter may have a default `name: T = expr`. A required parameter may not follow a defaulted one (E0026).

```noeta
fn greet(name: string, greeting: string = "Hello"): string {
    return "${greeting}, ${name}!"
}
echo greet("Ada")         // Hello, Ada!
echo greet("Ada", "Hi")   // Hi, Ada!
```

> [!NOTE]
> A default is evaluated in the function's **sealed definition scope**: it sees statics and the fn's `use (…)` captures — exactly like the body — but *not* other arguments, `self`, or fields (naming another parameter is E0005). A default that reads a module-level binding therefore needs the binding in the capture clause: `fn f(x: int, step: int = base) use (base)`. A default widens the accepted arity to a range.

## Multiple return via tuples

Return a [tuple](Structs-Classes-and-Enums#tuples) to hand back several values, and destructure at the call site:

```noeta
fn divmod(a: int, b: int): (int, int) {
    return (a / b, a % b)
}
(q, r) = divmod(17, 5)   // q = 3, r = 2
```

## Closures

A closure is `fn(params) => expr` (arrow) or `fn(params) { … }` (block, with `return`). Unlike named functions, a closure's parameter and return types are **optional** — they are inferred:

```noeta
base     = 100
add_base = fn(x) => x + base            // arrow; captures `base`
sumsq    = fn(xs) { mut t = 0; for x in xs { t = t + x * x }; return t }   // block body
classify = fn(n): string { if n > 0 { return "pos" }; return "nonpos" }    // return annotation
g        = fn(n: int, bump: int = 10) => n + bump                          // annotated + default
twice    = fn(f, x) => f(f(x))          // higher-order
```

Closures **capture their environment**, including `mut` cells that outlive the frame that created them:

```noeta
fn make_counter(): () -> int {
    mut n = 0
    return fn() { n = n + 1; return n }   // captures and mutates `n`
}
c = make_counter()
echo c()   // 1
echo c()   // 2
```

## Nested functions

A `fn` declared inside another function's body is **nested**: it is a local closure, callable only within the enclosing body, that captures enclosing locals as upvalues. Nested-function *names* are **hoisted** across their block — like top-level functions, siblings see each other regardless of declaration order — so forward references and mutual recursion just work:

```noeta
fn parity(n: int): bool {
    fn even(k: int): bool {          // calls `odd`, declared below
        if k == 0 { return true }
        return odd(k - 1)
    }
    fn odd(k: int): bool {           // calls `even`, declared above
        if k == 0 { return false }
        return even(k - 1)
    }
    return even(n)                   // mutual recursion, both directions
}
echo parity(10)   // true
```

Two mutually recursive nested functions may also import the *same* enclosing `mut` local with `use (…)` — they share the one live cell, not copies:

```noeta
fn run(): int {
    mut hits = 0
    fn ping(n: int) use (hits): int { hits = hits + 1; if n == 0 { return 0 }; return pong(n - 1) }
    fn pong(n: int) use (hits): int { hits = hits + 1; if n == 0 { return 0 }; return ping(n - 1) }
    ping(4)
    return hits   // 5 — every bounce incremented the shared counter
}
```

Only `fn` declarations are hoisted. A plain `let`/value local stays **strictly lexical**: referencing one declared textually later is E0005 (unknown name), not a forward capture.

```noeta error
fn run(): int {
    fn peek(): int { return later }   // E0005 — `later` is a value local, not hoisted
    later = 5
    return peek()
}
```

## Function types

Function types are first-class surface syntax — write them in annotations and signatures:

```noeta
apply: (int) -> int = fn(x) => x + 1
fn run(f: (int) -> int, x: int): int { return f(x) }
```

Collection methods are passable as values via **unbound method handles**: `list.len`, `string.upper`, `Stack.size` — each a callable taking the receiver as its first argument (`xss.map(list.len)`).

**Generic functions are values too.** With an expected function type in play — a `map` argument, an annotated binding, a declared `Fn`-typed field or parameter — a generic function instantiates against the expectation and checks precisely; without one it stays the erased, `dyn`-parameter value (calls defer per position):

```noeta
fn wrap<T>(x: T): List<T> { return [x] }

echo [1, 2].map(wrap)                 // [[1], [2]] — precise: (int) -> List<int>
g: (string) -> List<string> = wrap    // instantiates from the annotation
h = wrap                              // bare: the erased value, still callable at any type
```

The prelude constructors `Ok`/`Err`/`some` — and `panic` — are first-class the same way: `results.map(Ok)` passes the genuine constructor, with a direct call's exact arity behavior and error text. (`assert` stays a special form, and a generic function that forwards `T` into a call-site-typed position — see [Generics & Traits](Generics-and-Traits) — must be called rather than passed.)

## Calling a closure-valued field

A function value stored in a **field** is called directly through its receiver — `obj.f(args)` means `(obj.f)(args)` when `f` is a field of function type (the field-access-then-call desugar). The call is arity/argument-checked against the field's declared type, exactly like a call through a `Fn`-typed local:

```noeta
struct Counter {
    step: (int) -> int
}
c = Counter { step: fn(x: int) => x + 1 }
echo c.step(41)      // 42 — load the field, call the value
```

When a type declares **both** a method and a field of the same name, the method wins in call position (`obj.f(x)` dispatches the method) while the field wins in value position (`obj.f` reads the field) — bind it (`g = obj.f; g(x)`) to call the field. Parentheses are transparent, so `(obj.f)(x)` is the same call as `obj.f(x)`. On a `dyn` receiver the same order applies at runtime: the method table first, then the field. A field whose type is not a function is not callable — E0007, statically when the receiver's type is known.

## The pipe operator

`|>` threads the left value as the **first argument** of the right call. It turns nested calls into a left-to-right pipeline:

```noeta
fn inc(x: int): int { return x + 1 }
fn add(a: int, b: int): int { return a + b }

echo 5 |> inc |> inc          // inc(inc(5))
echo 5 |> add(10)             // add(5, 10)

echo [1, 2, 3, 4]
    .filter(fn(n) => n % 2 == 0)
    .map(fn(n) => n * 10)
    .sum()                    // 60  (collection work chains as methods)
```

## See also

- [Error Handling](Error-Handling) — `?` propagation and `Result`-returning functions.
- [Generics & Traits](Generics-and-Traits) — generic and bounded functions (`fn max<T: Comparable>`).
- [Concurrency](Concurrency) — `async fn`.
