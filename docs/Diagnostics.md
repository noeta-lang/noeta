# Diagnostics — the `E0xxx` catalog

Every diagnostic the toolchain reports carries a stable code. This page is the index: what each code means, and which page explains the rule behind it.

Codes are **permanent**. A code is assigned once and never reused or renumbered, so `E0059` means the same thing in every release, in a diagnostic, in a conformance case, and in a search.

## Reading one

Every stage of the compiler emits diagnostics through one renderer, so they all have the same shape — code, message, the span under a caret, and often a `help:` line carrying the fix:

```text
[E0059] Error: `base` is already bound in an enclosing scope
   ╭─[ app.noe:4:12 ]
   │
 4 │     scaled = fn(base) => base * 2
   │                 ──┬─
   │                   ╰── this binder reuses a name that is already in scope
   │
   help: rename this binder — one name means one thing per scope stack
───╯
```

Three codes are **warnings**: they print, but they do not fail a build — [E0063](#types-inference-and-narrowing), [E0065](#types-inference-and-narrowing), and [E0071](#directives-attributes-and-tiers). Everything else is an error.

Most codes are decided before your program runs, so [`noeta check`](The-CLI#noeta-check) reports them without executing anything. The ones that can only be known while running are marked **runtime** below.

## Names and scope

| Code | Meaning | Covered on |
|---|---|---|
| `E0005` | A name does not resolve to anything in scope. | [Functions & Closures](Functions-and-Closures#sealed-functions--the-use--capture-clause) |
| `E0006` | Assignment to an immutable binding — one not declared `mut`. | [Syntax Basics](Syntax-Basics#bindings-and-mutability) |
| `E0019` | A `use` names something the module does not export, or exports without `pub`. | [Modules](Modules#visibility--pub) |
| `E0020` | An imported name collides with another top-level name in the file. | [Modules](Modules#qualified-references) |
| `E0046` | A name is already spoken for — a reserved word (any keyword, `true`/`false`, or a reflection primitive such as `type_name`), or one of the prelude names `Ok`, `Err`, `some`, `none`, `panic`, `assert`. | [Syntax Basics](Syntax-Basics#reserved-words) |
| `E0049` | A declaration binds a reserved native type name, such as `Uuid` or `Iterator`. | — |
| `E0059` | A binder reuses a name already meaning something in scope. One name, one meaning, per scope stack. | [Syntax Basics](Syntax-Basics#bindings-and-mutability) |

## Types, inference, and narrowing

| Code | Meaning | Covered on |
|---|---|---|
| `E0007` | A type or arity mismatch — the language's general "this is not that" code. | [Syntax Basics](Syntax-Basics#bindings-and-mutability) |
| `E0013` | A type annotation names a type that does not resolve. | [The Type System](Type-System#type-forms) |
| `E0022` | A named function is missing a parameter or return type. Signatures are mandatory at named boundaries. | [The Type System](Type-System#where-inference-stops) |
| `E0023` | A binding's type cannot be inferred and is not annotated — `x = []`, `m = {}`. | [The Type System](Type-System#where-inference-stops) |
| `E0028` | `.as<T>()` applied to a value whose static type is already concrete — there is nothing dynamic to narrow. | [The Type System](Type-System#type-tests-and-narrowing) |
| `E0043` | A bitwise or shift operator applied to a non-integer operand. | [Fixed-Width Integers](Fixed-Width-Integers#bitwise-and-shift-operators) |
| `E0044` | A fixed-width integer literal outside its width's range, or two widths mixed in one expression. | [Fixed-Width Integers](Fixed-Width-Integers#the-fixed-width-types) |
| `E0063` | **Warning.** `x is i32`/`x is f64` on a scalar — those widths erase at runtime, so the test is always false. Test the base type. | [Fixed-Width Integers](Fixed-Width-Integers#the-fixed-width-types) |
| `E0065` | **Warning.** `x is T` on an `Option`/`Result` against its payload type — the value's head is `some`/`none`/`Ok`/`Err`, so the test is always false. Take the container apart instead. | [The Type System](Type-System#optionals--t) |

## Control flow and matching

| Code | Meaning | Covered on |
|---|---|---|
| `E0011` | A `match` does not cover every case and has no catch-all. | [Control Flow & Matching](Control-Flow-and-Pattern-Matching#match) |
| `E0024` | `break` or `continue` outside any loop. | [Control Flow & Matching](Control-Flow-and-Pattern-Matching#break-and-continue) |
| `E0055` | A block-bodied match arm where the `match`'s value is used. Blocks yield no value. | [Control Flow & Matching](Control-Flow-and-Pattern-Matching#arm-bodies-expressions-vs-blocks) |
| `E0066` | A match arm after an unguarded catch-all — it can never run. | [Control Flow & Matching](Control-Flow-and-Pattern-Matching#arm-order--an-arm-after-a-catch-all-is-dead-e0066) |

## Functions and calls

| Code | Meaning | Covered on |
|---|---|---|
| `E0026` | A required parameter follows a defaulted one. Defaults are trailing-only. | [Functions & Closures](Functions-and-Closures#default-optional-parameters) |
| `E0047` | A method called through the wrong receiver — an instance method as `Type.m(…)`, or an associated function on a value. | [Generics & Traits](Generics-and-Traits#implementing-a-trait) |
| `E0048` | A non-`void` function can reach the end of its body without returning. | [Functions & Closures](Functions-and-Closures#returning-on-every-path) |
| `E0058` | Wrong type arguments in a generic application — a turbofish that cannot apply, a built-in constructor at the wrong arity, or a name-keyed reflection query given a type *parameter*. | [Generics & Traits](Generics-and-Traits#explicit-instantiation--the-turbofish) |
| `E0061` | A named argument names no parameter, names one already supplied, or follows a positional with no position left. | [Functions & Closures](Functions-and-Closures#named-arguments) |

## Data: structs, classes, enums, fields

| Code | Meaning | Covered on |
|---|---|---|
| `E0009` | An all-fields literal left a declared field unset. | [Structs, Classes & Enums](Structs-Classes-and-Enums#constructing-values) |
| `E0033` | Assignment to a field not declared `mut`, or on a receiver with no assignable fields. | [Structs, Classes & Enums](Structs-Classes-and-Enums#fields) |
| `E0034` | `===`/`!==` on a non-reference operand. Identity is a `class`-only concept. | [Structs, Classes & Enums](Structs-Classes-and-Enums#the-valuereference-distinction) |
| `E0035` | A private field read, written, or set from outside its declaring type. | [Structs, Classes & Enums](Structs-Classes-and-Enums#fields) |
| `E0038` | `@packed` on a non-struct, or a packed struct with a field that cannot lay out flat. | [Fixed-Width Integers](Fixed-Width-Integers#packed-value-types--packed) |
| `E0060` | A `@validated` type built by literal from outside its own methods. Go through a constructor. | [Validation](Validation#validated--channeling-construction) |

## Traits, generics, and derives

| Code | Meaning | Covered on |
|---|---|---|
| `E0014` | An `impl` or `@derive` names a trait that is not known, or at the wrong generic arity. | [Generics & Traits](Generics-and-Traits#the-built-in-traits) |
| `E0015` | An `impl` does not satisfy the trait it names — a missing method, or one whose signature (arity, types, `async`-ness) disagrees. | [Generics & Traits](Generics-and-Traits#implementing-a-trait) |
| `E0025` | A type parameter instantiated with a type that does not satisfy its declared bound. | [Generics & Traits](Generics-and-Traits#bounds) |
| `E0027` | More than one implementation of the same trait for one type. | [Generics & Traits](Generics-and-Traits#uniqueness) |
| `E0050` | A `@derive` the type's fields cannot support — ordering a field kind with no ordering, say. | [Derives](Derives#field-constraints-e0050) |
| `E0053` | A malformed `trait` declaration — a duplicate trait name, a collision with a declared type, or a repeated method signature. | — |
| `E0070` | A standalone `impl Trait for Type` in a package that declares neither the trait nor the type. | [Generics & Traits](Generics-and-Traits#the-orphan-rule) |

## Errors and results

| Code | Meaning | Covered on |
|---|---|---|
| `E0012` | `?` on something that is not a `Result` or `Option`, or where the early return has nowhere to go. | [Error Handling](Error-Handling#the-enclosing-function-has-to-be-able-to-return-what--returns) |
| `E0057` | `?` would propagate an `Err` whose type neither matches the declared error type nor converts into it. | [Error Handling](Error-Handling#converting-errors-at---impl-fromsource) |
| `E0069` | **Runtime.** An `Err` propagated out of the top level. The program aborts with the error's `message()` and a non-zero exit. | [Error Handling](Error-Handling#the-enclosing-function-has-to-be-able-to-return-what--returns) |

## Modules

| Code | Meaning | Covered on |
|---|---|---|
| `E0072` | A file in a package declares a `namespace`. A derived path cannot be declared. | [Modules](Modules#a-modules-path-cannot-be-declared) |
| `E0073` | Two files derive the same module path. One path is one module. | [Modules](Modules#one-path-one-module) |
| `E0074` | A directory name or file stem is not a legal module-path segment. | [Modules](Modules#naming-a-file) |

## Directives, attributes, and tiers

| Code | Meaning | Covered on |
|---|---|---|
| `E0017` | A `#[…]` data attribute is malformed or misused. | [Derives](Derives#derive-errors) |
| `E0029` | `#[Foo]` names a type that is not a struct marked `@attribute`. | [Attributes & Reflection](Attributes-and-Reflection#attribute--mark-a-struct-usable-as-) |
| `E0030` | An attribute attached to a declaration kind it does not permit. | [Attributes & Reflection](Attributes-and-Reflection#attribute--mark-a-struct-usable-as-) |
| `E0031` | A malformed `@role` — an unknown role, the wrong count, or a struct that is not itself an attribute. | [Attributes & Reflection](Attributes-and-Reflection#roleenumvariant--a-semantic-role-tag) |
| `E0036` | An `@name` that resolves to nothing: not a built-in directive, not a tier, not one any installed extension declares. | [Dev Tiers](Dev-Tiers#directive-arguments-and-diagnostics) |
| `E0037` | An invalid directive argument — unknown parameter, wrong type, set twice, or supplied to a directive that takes none. | [Dev Tiers](Dev-Tiers#directive-arguments-and-diagnostics) |
| `E0051` | An invalid `@tier` declaration — a colliding name, a `config` that is not an `@attribute` struct, or a wrong runner signature. | [Extending Tiers](Extending-Tiers#declaring-your-own-tier) |
| `E0052` | An expression-tier block misused — a non-`expr:` tier in expression position, or an expression tier in statement position. | [Extending Tiers](Extending-Tiers) |
| `E0054` | A directive attached at a site the tier or directive does not permit. | [Attributes & Reflection](Attributes-and-Reflection#semantic--promote-an-enum-to-a-role-vocabulary) |
| `E0062` | An extension's directive **expansion hook** failed, or generated code that does not parse. Blamed on the directive. | [The CLI](The-CLI#noeta-expand) |
| `E0071` | **Warning.** A tier runner left a top-level statement out of the shared setup, and a selected test captures a binding that statement writes. | [Testing](Testing#what-runs-and-what-does-not) |

## Concurrency and reactivity

| Code | Meaning | Covered on |
|---|---|---|
| `E0039` | A generator misuse — `yield` outside a generator, a declared return that is not `Iterator<T>`, or a value in a generator's `return`. | [Concurrency](Concurrency#generators--yield) |
| `E0040` | An async misuse — `.await` outside an async context, on a non-future, in a closure, or in a condition or loop head. | [Concurrency](Concurrency#async--await) |
| `E0041` | A `spawn` outside any `concurrent { }` scope, or a `spawn`/`isolate` on an operand that is not a future — the callee must be an `async fn`. | [Concurrency](Concurrency#structured-concurrency) |
| `E0042` | A `!Send` value would cross an isolate boundary. Only value types may cross. | [Concurrency](Concurrency#isolates-and-send) |
| `E0045` | **Runtime.** A reactive update did not converge — an effect keeps changing a signal it depends on. | [Reactivity](Reactivity#non-termination-is-caught-not-hung) |
| `E0056` | **Runtime.** `.await` on a **cancelled** task. A cancelled task never produces a value; use `h.join()` to observe the outcome. | [Concurrency](Concurrency#cancellation) |

## Syntax

| Code | Meaning |
|---|---|
| `E0001` | The lexer hit a character that cannot start a token. |
| `E0002` | A string literal was opened but never closed. |
| `E0003` | The parser expected one token and found another. Also what a reflection surface reports when handed the wrong kind of operand. |
| `E0004` | Input ended while a construct was still open. |
| `E0032` | Delimiters nest deeper than the parser supports. |
| `E0064` | A malformed string escape — a `\xHH` outside ASCII or without two hex digits, or a bad `\u{…}`. |

## Runtime failures

These describe the environment a well-formed program acted on, so they surface while it runs.

| Code | Meaning | Covered on |
|---|---|---|
| `E0008` | Integer division or remainder by zero. | — |
| `E0010` | A `panic(…)`, a failed `assert`, or a detected deadlock aborted the program. | [Error Handling](Error-Handling#panic-and-assert) |
| `E0016` | An index addressed a list, string, or `bytes` position out of bounds. | [Built-ins](Standard-Library) |
| `E0018` | `m[k]` addressed a map with a key it does not contain. | [Built-ins](Standard-Library#map) |
| `E0021` | An IO operation failed — reading a path that does not exist, say. | [Built-ins](Standard-Library) |

## Internal

| Code | Meaning |
|---|---|
| `E0068` | The backend could not compile a program the type checker accepted. This is a bug in the toolchain, not in your source — please [report it](https://github.com/noeta-lang/noeta/issues) with the program that produced it. |

## See also

- [The CLI](The-CLI#noeta-check) — `noeta check` reports every diagnostic without running anything; `--format json` emits them machine-readably.
- [Editor & AI Tooling](Editor-and-AI-Tooling) — the language server surfaces these live, and `noeta mcp`'s `explain_diagnostic` tool serves a code's explanation with programs that trigger and fix it.
- [Architecture & Pipeline](Architecture-and-Pipeline#diagnostics-as-data) — why every diagnostic is a typed value with one renderer.
