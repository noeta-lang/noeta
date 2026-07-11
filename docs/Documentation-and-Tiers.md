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

### Attachment — docs belong to declarations

A `@doc` block **attaches by adjacency**: immediately above a declaration (`fn`/`struct`/`class`/`enum`), it documents that declaration. A non-attached block (above the `use` header, between sections, or standing alone) is the **module doc** if it is the file's first such block, else free-floating section prose. No new syntax — position decides.

Attachment feeds the whole toolchain from one resolution:

- **Hover** (LSP): hovering a documented symbol — its declaration or any call site — shows the doc prose under the type.
- **`noeta doc`**: an attached block's source header carries the symbol (see below).
- **Runtime docstrings**: with the `doc` tier live (`noeta run --tier doc`), the attached block is stamped as the prelude `#[Doc(text: "…")]` attribute on its declaration, so `attributes_of::<Doc>()` surfaces it at runtime — Python-style docstrings, opt-in. On a normal build nothing is stamped and the blocks strip as always, so **production carries no doc text**.

Extract every `@doc` block to stdout:

```console
$ noeta doc adder.noe
<!-- adder.noe:1 -->
# Adder

<!-- adder.noe:5 · add -->
`add(a, b)` returns **a + b**. Pure; no side effects.
```

- The program is **not** type-checked or run — docs extract from a parse alone, so you can pull docs from work-in-progress code.
- Each block is **dedented** (leading/trailing blank lines dropped, common indentation stripped) and prefixed with an HTML-comment source header (`<!-- file:line -->`, plus `· symbol` for an attached block), which is valid Markdown that renders to nothing.
- No `@doc` blocks → a notice on stderr, exit `0`.

```
noeta doc [OPTIONS] <FILE>
```

The only flag is `--target <NAME>`, which gates extraction on the `doc` tier being live in that build target.

---

## The tier model

There are two orthogonal ideas:

- A **tier** is a *kind of co-located content* — a property of the **source**. Built-in tiers: `test`, `bench`, `debug` (all *code*), and `doc` (*text*) — declared by std's core extension through the same `ExtTier` surface a third-party extension uses (only the runners are native); the name-space is open (see [Declaring your own tier](#declaring-your-own-tier)).
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
> The table shape leaves room for a target to carry platform/artifact keys later (the full build recipe).

---

## Declaring your own tier

The tier name-space is **open**: a package (or the program itself) brings a new tier into existence with a `@tier` declaration on its **runner** function:

```noeta
namespace fuzz.tiers;

@attribute(Function)
pub struct Fuzz { cases: int }          // the tier's knobs, as an ordinary attribute

@tier(fuzz, config: Fuzz)
pub fn run_fuzz(roots: List<TierRoot>): void {
    configs = attributes_of::<Fuzz>()   // knob values, per root, via reflection
    for root in roots {
        run = root.run
        run()                           // invoke the root — an in-process fn handle
    }
}
```

- `@tier(name[, config: Type])` names the tier consumers write as `@<name> { … }`; the optional `config:` names an `@attribute` struct whose fields are the tier's knobs — exactly the `Bench { iterations }` model, so `@fuzz(cases: 500) { … }` stamps `#[Fuzz(cases: 500)]` onto each contained fn and the ordinary construction gate validates it.
- The decorated fn is the **runner**: it must be `fn(roots: List<TierRoot>): void`, where each `TierRoot { name: string, run: () -> void }` is an activated root fn as a first-class handle. Anything else about the declaration — a name colliding with a built-in, a duplicate, a non-attribute config, a wrong signature — is **E0051** at the declaration.
- A consumer opts in with one import (`use fuzzkit.tiers.run_fuzz` — the config struct links along with the runner), writes `@fuzz { … }` blocks, and runs **`noeta fuzz <file>`**: the unknown subcommand resolves against the file's declared tiers and dispatches to the runner in-process, after the compose and before the `noeta-<cmd>` external-binary probes. Roots keep white-box access and strip from a normal build, exactly like the built-in tiers.
- In `noeta.toml`, any identifier is a valid tier name in a target's `tiers` map — whether it resolves is checked where the tier is used.

---

## Related: the decorator directives

The `@<tier>` blocks above are distinct from the four **decorator directives** — `@derive`, `@attribute`, `@role`, `@semantic` — which annotate *declarations* rather than gate content. Those are language features, covered in [Attributes & Reflection](Attributes-and-Reflection).

## See also

- [Testing](Testing) · [Benchmarking](Benchmarking) — the runnable tiers.
- [The `noeta` CLI](The-CLI) — the commands that drive tiers.
