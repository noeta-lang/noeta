# Documentation & Dev Tiers

`@test`, `@bench`, `@doc`, and `@debug` are **dev tiers** — kinds of content you co-locate with your code that are stripped from a normal build and activated only by the right tool. This page covers `@doc` (extractable prose), the tier model that unifies all four, and the `noeta.toml` build targets that decide which tiers are live.

---

## `@doc` — prose that lives with the code

A `@doc { … }` block holds verbatim prose (Markdown). Its body is captured **un-parsed** by the lexer, so Markdown punctuation — `#`, `*`, backticks, `%` — never lexes as language tokens. The surrounding program still compiles and runs normally.

```noeta
@doc {
    # Adder

    `add(a, b)` returns **a + b**. Pure; no side effects.
}

fn add(a: int, b: int): int { return a + b }
```

Extract every `@doc` block to stdout:

```console
$ noeta doc adder.noe
<!-- adder.noe:1 -->
# Adder

`add(a, b)` returns **a + b**. Pure; no side effects.
```

- The program is **not** type-checked or run — docs extract from a parse alone, so you can pull docs from work-in-progress code.
- Each block is **dedented** (leading/trailing blank lines dropped, common indentation stripped) and prefixed with an HTML-comment source header (`<!-- file:line -->`), which is valid Markdown that renders to nothing.
- No `@doc` blocks → a notice on stderr, exit `0`.

```
noeta doc [OPTIONS] <FILE>
```

The only flag is `--target <NAME>`, which gates extraction on the `doc` tier being live in that build target.

---

## The tier model

There are two orthogonal ideas:

- A **tier** is a *kind of co-located content* — a property of the **source**. Built-in tiers: `test`, `bench`, `debug` (all *code*), and `doc` (*text*).
- A **target** is a *named build recipe* — a property of the **build invocation** (in `noeta.toml`) — that decides which tiers are live: a `dev` target includes them, a `prod` target strips them all.

### How activation works

On a normal `noeta run`, the active-tier set is empty: every `@<tier> { … }` block is **stripped before lowering**. It never reaches the type checker or either backend, so tier content can never affect a production build — the stripping is by construction, not a dead-code pass.

When a tier *is* active, its block's items are **inlined** into the top-level program (the block is pure grouping sugar), and the lifted functions gain white-box access to private fields (see [Testing](Testing)). A block is resolved wherever it appears — top-level *and* nested inside a function body, loop, or branch.

Each tool activates its own tier: `noeta test` activates `test`, `noeta bench` activates `bench`, `noeta doc` activates `doc`. The `debug` tier has no dedicated command — you activate it explicitly.

### `@debug` — conditional inline code

`@debug { … }` is code in *statement* position: instrumentation you want compiled in only sometimes.

```noeta
fn f(x: int): void {
    @debug { echo "debug: x is ${x}" }
    echo "result: ${x * 2}"
}
```

```console
$ noeta run prog.noe              # @debug stripped
result: 10
$ noeta run prog.noe --tier debug # @debug compiled in
debug: x is 5
result: 10
```

`--tier <NAME>` is repeatable and unions with any `--target`.

### The annotation form

`@<tier> fn …` is exactly a one-item block — sugar for wrapping a single function:

```noeta
@test fn adds(): void { assert(add(1, 2) == 3) }
```

### Directive arguments and diagnostics

A tier directive can take arguments — `@bench(iterations: 1000)` (or positional `@bench(1000)`). A tier's knobs are declared as a prelude **config attribute** (`bench`'s is `Bench { iterations: int }`), and the directive args are distribution sugar: the block stamps `#[Bench(iterations: 1000)]` onto each contained fn that does not already carry one (a per-fn attribute wins). Validation is therefore the ordinary attribute construction gate — an unknown parameter, duplicate, or wrong type reports the same construction diagnostics (`E0005`/`E0007`/`E0009`) as any `#[…]` attribute, and the knobs are reflectable via `attributes_of`.

| Code | Meaning |
|---|---|
| **E0036** UnknownTier | `@<name>` names a tier that is not built-in (e.g. `@tset`). Raised whether or not it would be active — a typo must never silently vanish. |
| **E0037** InvalidDirectiveArgument | Arguments on a tier with no config attribute (`@test(x)` — `test` takes no arguments). |

---

## Build targets — `noeta.toml`

A `noeta.toml` at (or above) your entry file's directory defines named build targets. Each maps tier names to the package that provides them:

```toml
[targets.dev.tiers]
test  = "std"
bench = { package = "std", samples = 100 }
debug = "std"

[targets.ci]
extends = "dev"
[targets.ci.tiers]
doc = "std"
```

- A target's **active tiers** are the tier names in its (inheritance-merged) map.
- `extends = "<base>"` inherits another target's tiers; the child's own entries override the base's. Cycles are detected and rejected.
- `--target <NAME>` on `noeta run` activates those tiers (unioned with `--tier`). On `noeta test`/`bench`/`doc`, `--target` acts as a **gate** — the tool no-ops if the target does not make its tier live.

> [!NOTE]
> **What works today vs. what is stubbed.** The full target grammar works now: parsing, `extends` inheritance, tier-name validation, provider string-vs-table forms, cycle detection, and unknown-target errors. A tier's provider may be the built-in `"std"` or any declared `[dependencies]` key — but naming a third-party provider is not yet *consumed*: the built-in runners always run. Wiring provider dispatch (and `@tier` declarations, so packages can define new tiers) is the remaining step; the table shape also leaves room for a target to carry platform/artifact keys later.

---

## Related: the decorator directives

The `@<tier>` blocks above are distinct from the four **decorator directives** — `@derive`, `@attribute`, `@role`, `@semantic` — which annotate *declarations* rather than gate content. Those are language features, covered in [Attributes & Reflection](Attributes-and-Reflection).

## See also

- [Testing](Testing) · [Benchmarking](Benchmarking) — the runnable tiers.
- [The `noeta` CLI](The-CLI) — the commands that drive tiers.
