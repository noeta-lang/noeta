# Functions & Closures

Named functions, default parameters, multiple return, closures, and the pipe operator.

## Named functions

`fn name(params): Ret { body }`. At this named boundary, **parameter types and the return type are mandatory**, and a missing type is E0022. Bodies infer everything else.

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

### Returning on every path

A non-`void` function must produce its declared type on **every** path, and reaching the end of the body without returning is `E0048`. The checker proves a body cannot reach its end from these shapes, nested to any depth:

- a `return`, or a `panic(…)`;
- an `if`/`else` where *both* blocks do;
- a `while true { … }` with no `break` targeting it;
- an **exhaustive** `match` whose every arm does.

The `match` case matters because blocks never yield values in Noeta. An arm that has to bail out early is written as a block with a `return` in it, which makes an all-returning `match` the ordinary way to write a fallible pipeline, with no unreachable trailing `return` to pad it out:

```noeta
fn unwrap_or_zero(r: Result<int, string>): int {
    match r {
        Ok(v) => { return v },
        Err(_) => { return 0 },
    }
}
```

"Exhaustive" is the same judgment `E0011` reports on: a `_` or bare-binding arm, or an arm for every variant of an enum, `Result`, or `Option`. A **guarded** arm (`pattern if cond`) proves nothing, the guard being able to fail at runtime, so a `match` that relies on one still needs a later irrefutable arm. A `match` over an open domain, an `int` scrutinee say, needs a `_`. Anything the checker cannot prove exhaustive leaves a path to the end of the body, and `E0048` still fires there.

## Sealed functions & the `use (…)` capture clause

A named function is **sealed**. Its body sees its parameters and the program's *declarations*, meaning other functions, types, and imports, and never the surrounding **value bindings**. A top-level `items` does not leak into `fn place(items: …)`, and inside the body `items` means the parameter. To read a surrounding binding, import it explicitly with a capture clause between the parameter list and the return type:

```noeta
tax_rate = 0.25
fn with_tax(price: float) use (tax_rate): float {
    return price * (1.0 + tax_rate)
}
echo with_tax(100.0)   // 125.0
```

A capture is a **live view** of the named binding. A `mut` global mutated later is seen, and writing through the capture follows the binding's own `mut` rules. The clause works on methods and nested `fn`s the same way, so a nested `fn` importing an enclosing `mut` local is the explicit form of a closure counter.

Without the clause, a reference to a top-level binding is E0005 with the fix spelled out (*add `use (name)` to the signature, or pass it as a parameter*), and a bare assignment to an unlisted name declares a fresh local.

**No shadowing (E0059).** One name means one thing per scope stack, and the rule in full lives in [Syntax Basics](Syntax-Basics#bindings-and-mutability). Sealing is what keeps it ergonomic here: named-fn params conflict with nothing because the surrounding bindings genuinely are not in scope. An anonymous closure captures implicitly, so `fn(base) => …` under a visible `base` is rejected; rename one.

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
> A default is evaluated in the function's **sealed definition scope**, so it sees statics and the fn's `use (…)` captures exactly as the body does, and not other arguments, `self`, or fields (naming another parameter is E0005). A default that reads a module-level binding therefore needs the binding in the capture clause: `fn f(x: int, step: int = base) use (base)`. A default widens the accepted arity to a range.

## Named arguments

An argument may name the parameter it fills, `name: value`. Positional arguments come first, and once an argument is named every argument after it must be named too. `f(a: 1, 2)` is rejected, the labels having already claimed parameters out of order, so `2` has no position left to take. Named arguments themselves may appear in any order:

```noeta
fn sub(a: int, b: int): int { return a - b }

echo sub(10, 1)          // 9  — positional
echo sub(a: 10, b: 1)    // 9  — labelled
echo sub(b: 1, a: 10)    // 9  — labelled, any order
```

Labels bind, and they do not reorder evaluation. Arguments are evaluated in the order **written**, so a call's side effects never depend on how its parameters happen to be declared:

```noeta
fn sub(a: int, b: int): int { return a - b }

fn trace(tag: string, v: int): int {
    echo tag
    return v
}

// sample:start
echo sub(b: trace("b", 1), a: trace("a", 10))   // prints b, then a, then 9
// sample:end
```

Because a label names its parameter, a call can supply a later defaulted parameter while leaving an earlier one to its default:

```noeta
fn f(a: int, b: int = 2, c: int = 3): int { return a * 100 + b * 10 + c }

echo f(1)          // 123 — every default
echo f(1, 5)       // 153 — `b` supplied positionally
echo f(1, c: 9)    // 129 — `b` skipped, and still defaults
```

The skipped parameter's default is evaluated by the callee in its own scope, exactly as when an argument list simply stops early. Skipping one and omitting a trailing one are the same thing to the function being called.

Named arguments work the same on methods and static functions (`m.f(1, c: 9)`, `M.mk(b: 5, a: 1)`), and through [the pipe operator](#the-pipe-operator).

A label must name a parameter of the callee, an unknown one being E0061 with the closest match suggested, and no parameter may be filled twice, once positionally and again by name.

### Where labels can be used

A label binds against the callee's declared parameter **names**, so it works wherever those names are visible. That covers functions, methods, and static functions declared in Noeta, and **standard-library functions**, whose signatures declare names too:

```noeta
use std.math

// sample:start
echo math.pow(2.0, 3.0)              // 8.0 — positional
echo math.pow(base: 2.0, exp: 3.0)   // 8.0 — labelled
echo math.pow(exp: 3.0, base: 2.0)   // 8.0 — labelled, reordered
// sample:end
```

Where a label **cannot** bind it is refused (E0061) rather than ignored:

- A **function value**, meaning a closure stored in a binding, field, or parameter. The closure literal had parameter names, but the `(int, int) -> int` type it flows through carries only types, so the call site cannot see them. No signature can fix this one.
- A **built-in method** on a primitive or collection (`"s".replace`, `xs.map`), and any native signature that has not declared names. These resolve from the receiver's type rather than from a named signature.

A label is always honored or refused, never silently ignored. So `math.pow(exp: 3.0, base: 2.0)` cannot quietly compute 3², and a label naming nothing at all (`"abc".replace(zzz: "a", "b")`) cannot pass unremarked.

> [!NOTE]
> A call that *skips* a defaulted parameter can only name parameters among the first 63, and one that names a later parameter as well is rejected. The bound applies to skipping alone, so a call that fills a prefix of the parameters reorders and labels freely at any arity.

## Multiple return via tuples

Return a [tuple](Structs-Classes-and-Enums#tuples) to hand back several values, and destructure at the call site:

```noeta
fn divmod(a: int, b: int): (int, int) {
    return (a / b, a % b)
}
(q, r) = divmod(17, 5)   // q = 3, r = 2
```

## Closures

A closure is `fn(params) => expr` in arrow form, or `fn(params) { … }` in block form with `return`. A closure's parameter and return types are **optional** and inferred, where a named function's are mandatory:

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

A `fn` declared inside another function's body is **nested**. It is a local closure, callable only within the enclosing body, capturing enclosing locals as upvalues. Nested-function *names* are **hoisted** across their block, so, as with top-level functions, siblings see each other regardless of declaration order and forward references and mutual recursion just work:

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

Two mutually recursive nested functions may also import the *same* enclosing `mut` local with `use (…)`. They share the one live cell:

```noeta
fn run(): int {
    mut hits = 0
    fn ping(n: int) use (hits): int { hits = hits + 1; if n == 0 { return 0 }; return pong(n - 1) }
    fn pong(n: int) use (hits): int { hits = hits + 1; if n == 0 { return 0 }; return ping(n - 1) }
    ping(4)
    return hits   // 5 — every bounce incremented the shared counter
}
```

Only `fn` declarations are hoisted. A plain value local stays **strictly lexical**, so referencing one declared textually later is E0005, an unknown name, rather than a forward capture.

```noeta error
fn run(): int {
    fn peek(): int { return later }   // E0005 — `later` is a value local, not hoisted
    later = 5
    return peek()
}
```

## Function types

Function types are first-class surface syntax, written in annotations and signatures:

```noeta
apply: (int) -> int = fn(x) => x + 1
fn run(f: (int) -> int, x: int): int { return f(x) }
```

Collection methods are passable as values via **unbound method handles**: `list.len`, `string.upper`, `Stack.size`. Each is a callable taking the receiver as its first argument, so `xss.map(list.len)` works.

**Generic functions are values too.** With an expected function type in play, a `map` argument or an annotated binding, a generic function instantiates against the expectation and checks precisely. Without one it stays the erased, `dyn`-parameter value, with calls deferred per position. The full rules are in [Generics & Traits](Generics-and-Traits#generic-functions-as-values).

The prelude names are first-class the same way. `Ok`, `Err`, `some` and `panic` pass as genuine constructors, with a direct call's exact arity behavior and error text, so `results.map(Ok)` builds `[Ok(1), Ok(2)]`. `assert` passes too, and a handle bound from it raises the same E0010 with the same message a direct call would.

One shape needs the expectation. A generic function that forwards `T` into a call-site-typed position, such as one whose body calls `json.try_parse::<T>`, has to have its instantiation pinned by an expected function type (`g: (string) -> Result<U, JsonError> = decode`). Passing it where nothing pins the type is E0058, with the fix in the help.

## Calling a closure-valued field

A function value stored in a **field** is called directly through its receiver. `obj.f(args)` means `(obj.f)(args)` when `f` is a field of function type, this being the field-access-then-call desugar. The call is arity- and argument-checked against the field's declared type, exactly like a call through a `Fn`-typed local:

```noeta
struct Counter {
    step: (int) -> int
}
c = Counter { step: fn(x: int) => x + 1 }
echo c.step(41)      // 42 — load the field, call the value
```

When a type declares **both** a method and a field of the same name, the method wins in call position, so `obj.f(x)` dispatches the method, and the field wins in value position, so `obj.f` reads the field. Bind it (`g = obj.f; g(x)`) to call the field. Parentheses are transparent, making `(obj.f)(x)` the same call as `obj.f(x)`. On a `dyn` receiver the same order applies at runtime, the method table first and then the field. A field whose type is not a function is not callable, which is E0007, reported statically when the receiver's type is known.

## The pipe operator

`|>` threads the left value in as an **argument** of the right call, by default the first one. It turns nested calls into a left-to-right pipeline:

```noeta
fn inc(x: int): int { return x + 1 }
fn add(a: int, b: int): int { return a + b }

echo 5 |> inc |> inc          // inc(inc(5))
echo 5 |> inc()               // the same call, written with the empty list
echo 5 |> add(10)             // add(5, 10)

echo [1, 2, 3, 4]
    .filter(fn(n) => n % 2 == 0)
    .map(fn(n) => n * 10)
    .sum()                    // 60  (collection work chains as methods)
```

A callee that receives nothing but the piped value needs no argument list, so `5 |> inc` above is the whole call. Writing the empty parentheses is equivalent, so pick whichever reads better in the chain. The right-hand side is an ordinary expression either way: a method binds as `5 |> obj.m`, and so does anything that evaluates to a function, so `5 |> double` works for `double = fn(x: int) => x * 2`.

### Piping into a parameter that isn't the first

The piped value is the one argument with no written position, so it takes the first parameter **no label claimed**. Labelling the parameters you supply is therefore how you choose where the piped value lands:

```noeta
fn div(a: int, b: int): int { return a / b }

// sample:start
echo 100 |> div(b: 5)    // 20 — `b` is named, so the piped value fills `a`
echo 5 |> div(a: 100)    // 20 — `a` is named, so the piped value fills `b`
// sample:end
```

This composes with everything labels already do. A label may skip a defaulted parameter through a pipe, and the right-hand side's own positional arguments follow the piped value into whatever parameters are still free:

```noeta
fn f(a: int, b: int = 2, c: int = 3): int { return a * 100 + b * 10 + c }
fn g(a: int, b: int, c: int): int { return a * 100 + b * 10 + c }

// sample:start
echo 1 |> f(c: 9)        // 129 — piped into `a`; `b` still defaults
echo 5 |> g(6, c: 9)     // 569 — `c` is named, so `a` and `b` are free: piped, then 6
// sample:end
```

Evaluation order is unchanged by any of this. The left operand runs first, then the right-hand side's arguments in the order written, however the binding permutes them. A method binds the same way, with the receiver staying the receiver, so `10 |> box.scale(k: 4)` calls `box.scale(4, 10)`.

This needs a callee whose parameter names are visible ([where labels can be used](#where-labels-can-be-used)), which includes the standard library, so `2.0 |> math.pow(exp: 3.0)` pipes into `base` and gives `8.0`.

A label is the only way to thread the value into a later parameter, since there is no placeholder syntax and `_` is not a piped-value hole. Use a closure when the target parameter has nothing to name it by:

```noeta
fn div(a: int, b: int): int { return a / b }

// sample:start
echo 5 |> fn(n) => div(100, n)   // 20
// sample:end
```

## See also

- [Error Handling](Error-Handling) — `?` propagation and `Result`-returning functions.
- [Generics & Traits](Generics-and-Traits) — generic and bounded functions (`fn max<T: Comparable>`).
- [Concurrency](Concurrency) — `async fn`.
