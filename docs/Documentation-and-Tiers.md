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

```text
noeta doc [OPTIONS] <FILE>
```

### Generating a documentation artifact

`noeta doc <FILE> --out <DIR>` generates the **package documentation artifact** instead of extracting to stdout:

- **`docs.json`** — the canonical machine-readable form: schema-versioned, keyed by the package's `[package]` identity and version, modules with their namespace, module doc, and items in source order (sections woven between declarations). Deterministic — no timestamps, no absolute paths — so the artifact is content-addressable and **registry-ready**: a published package's docs can ride along and be rendered server-side.
- **`index.md` + one page per module** — a faithful Markdown rendering of the same data: each public declaration as a signature code block (carrying its `@tier`/`@attribute` directives) followed by its adjacency-attached prose.

A module that declares a `namespace` (a package module) documents its `pub` API only; a bare entry script documents every top-level declaration. Generation works from a bare parse — a sibling that fails to parse is skipped with a note, never fatal.

### Docs on the registry

`noeta publish` generates the package's `docs.json` and stores it **with the release** (skip with `--no-docs`; a docs failure warns, never blocks a publish). Fetch any published package's docs back:

```console
$ noeta doc --package acme/greeter            # highest version — docs.json to stdout
$ noeta doc --package acme/greeter@0.3.0 --out docs/   # pinned — render the Markdown tree
```

Stored docs are *advisory metadata*, not provenance: unsigned, last-wins on re-publish, and a hosted registry may regenerate them from source itself (the docs.rs model) rather than trust the upload.

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

### Target-scoped dependencies

A target can also carry its own dependencies, which **layer on top of** the global `[dependencies]` — the same overlay rule as tiers, so a dev-only tool never rides into a build that didn't ask for it:

```toml
[dependencies]                          # the default/base — present in every build
http = { registry = "acme/http" }

[targets.dev.dependencies]              # layered on only when this target is selected
lint = { registry = "acme/noeta-lint" }
```

The **global config is the default**: omit `--target` and a command sees `[dependencies]` and no tiers — the minimal, safe baseline. Put your shipping dependencies in the global config and keep dev-only tools/tiers in a `[targets.dev]` overlay you opt into with `--target dev`. There is no separate "dev vs prod" concept baked into the language — *you* decide what each target contains; the default build simply excludes anything you scoped under a target.

This is why a **shipped artifact is safe by default**: `noeta build` with no `--target` produces the global (baseline) build, and (as [The CLI](The-CLI#shipped-artifacts-are-lean-by-construction) covers) that artifact links only runtime code — never the dev toolchain. `--target dev` layers dev tiers/deps back in when you actually want them.

> [!NOTE]
> The table shape leaves room for a target to carry more of the build recipe later.

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

### Overriding a tier's provider

A target's `tiers` map is a **provider selection**, and it can re-point a built-in: with

```toml
[targets.custom.tiers]
bench = "fuzzkit"
```

`noeta bench app.noe --target custom` activates the `bench` tier against **fuzzkit's** `@tier(bench, config: …)` declaration — its config attribute is what `@bench(…)` blocks stamp, and its runner receives the roots — while a plain `noeta bench app.noe` keeps the native runner and std's `Bench { iterations }`. Declaring a tier under a built-in name is legal for exactly this reason: the declaration is dormant until a target selects its package (`E0051` now only rejects two declarations of one tier *within one package*). A provider that declares no such tier is an error naming both sides. `test`/`doc` override the same way — a `doc = "docgen"` provider is the documentation-site seam: activation stamps every attached block as `#[Doc]`, and the runner walks `attributes_of::<Doc>()`. The provider selection is part of the compile (and the startup-cache key), and `noeta <tier> <file> --target <name>` steers custom-tier dispatch the same way.

---

## Expression tiers — embedded languages as values

A tier declared with **`expr: Type`** turns its blocks into *expressions*: the body is verbatim foreign-language text with **`${…}` holes**, and each block evaluates to a typed value by calling the decorated fn — the tier's **handler** — with the body's pieces:

```noeta
@tier(greet, text: "text", expr: string)
fn render(statics: List<string>, holes: List<() -> string>): string {
    mut out = ""
    mut i = 0
    for s in statics {
        out = out ~ s
        if i < holes.len() {
            h = holes[i]
            out = out ~ h()
        }
        i = i + 1
    }
    return out
}

name = "world"
echo @greet { hello ${name}! }     // " hello world! "
```

`@greet { hello ${name}! }` desugars to `render(["hello ", "! "], [fn() => name])` — an ordinary call, which is where all the guarantees come from:

- **Holes are real expressions.** They parse with the full grammar, close over the enclosing scope, and type-check against the handler's declared hole type `U` — a mismatched `${…}` is an ordinary type error pointing *inside* the block. `${…}` follows string interpolation's contract exactly (same `\$` escape for a literal `$`; the text escapes `\{ \} \\` from text tiers also apply).
- **Statics always number holes + 1** (empty where holes touch), so the handler can interleave deterministically.
- **Holes are thunks.** Each desugars to a zero-param closure, so *whether and when* a hole evaluates is the handler's choice: call each once for an eager DSL, skip unused fragments, or wrap them in `computed`s for a reactive template.
- **The block's type is the handler's return type**, which must match the declared `expr:` (E0051 otherwise, like any broken tier declaration). The handler signature is fixed: `fn(statics: List<string>, holes: List<() -> U>): T` for your choice of `U`.

`text: "<lang>"` is optional on an expression tier but recommended — the language ID drives editor highlighting of the body. An expression tier has no runner semantics: its blocks never activate or strip, `noeta <tier>` rejects it, and a block in *statement* position (its value silently discarded) is **E0052** — assign or return it.

Because the declaration is ordinary code, a **pure-Noeta package** can ship `@sql`, `@json`, or `@html` — parsed, checked, typed embedded languages — with no native code and no compiler plugin: consumers `use` the handler's module and write blocks. See `examples/sql_tier.noe` for a small end-to-end DSL.

### Native (Rust-package) expression tiers

A **native** package declares an expression tier the same way it registers modules and types — through the extension ABI (`ExtTier`), naming the body language, the value type, and a **native handler** (a module function). The tier is then available wherever the package is installed, with no import of the handler, and its blocks are checked and typed like any expression. std dogfoods this with **`@json`**: a native handler (`std.template.render`) that interleaves the statics with JSON-quoted holes.

```noeta
id = "u-7"
name = "Ada Lovelace"
row = @json { {"id": ${id}, "name": ${name}} }   // a checked `string`
echo row                                          // {"id": "u-7", "name": "Ada Lovelace"}
```

The handler receives the hole thunks as closures and invokes them through the higher-order native capability, so a native tier can be as lazy as a Noeta one. Under the hood both handler kinds are the same thing — a function value the block's desugared call targets — so a native and a program-declared tier are indistinguishable to the checker, both backends, and the LSP.

### Editor support — language, highlighting, and the LSP

A tier's `text:` **is** the body's language, and it flows to the tooling three ways — the language is declared once, in the tier, and every consumer picks it up:

- **The LSP reports it.** Hovering an embedded block's tier name (`@sql { … }`) shows `expression tier @sql — sql body, evaluates to Query` — the declared language and the value type, read from the tier registry. The registry unions the program's own `@tier` declarations with any an installed extension contributes, so a program-declared tier and a native package's tier hover identically. (The block itself already hovers as its value type, `Query`, like any expression.)
- **Highlighting is extension-provided (VS Code / TextMate).** A package ships a TextMate injection grammar that colors its body as the foreign language, contributed with `injectTo: ["source.noeta"]`. It attaches by textual match (`injectionSelector: L:source.noeta`), so it needs no change to Noeta's own grammar — that is why an extension can provide it. `${…}` holes are scoped back to `source.noeta`, so they highlight as ordinary Noeta inside the foreign text — the same split the compiler makes (statics = foreign language, holes = checked Noeta). See `examples/sql_tier_injection.tmLanguage.json` for the shape.
- **tree-sitter** highlighting of third-party tiers needs a per-project generated grammar (a *static* grammar cannot read the declaration set to know which `@name` opens a verbatim body). The built-in `@doc` → markdown injection ships; a generalized generator is future work.

---

## Related: the decorator directives

The `@<tier>` blocks above are distinct from the four **decorator directives** — `@derive`, `@attribute`, `@role`, `@semantic` — which annotate *declarations* rather than gate content. Those are language features, covered in [Attributes & Reflection](Attributes-and-Reflection).

## See also

- [Testing](Testing) · [Benchmarking](Benchmarking) — the runnable tiers.
- [The `noeta` CLI](The-CLI) — the commands that drive tiers.
