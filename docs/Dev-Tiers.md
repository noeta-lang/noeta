# Dev Tiers

Tests, benchmarks, documentation and debug instrumentation live with the code they describe, inside `@test`, `@bench`, `@doc` and `@debug` blocks. Those blocks are **dev tiers**: co-located content that a normal build strips, and that the matching tool activates.

```noeta
fn add(a: int, b: int): int { return a + b }

@test fn adds(): void { assert(add(1, 2) == 3) }
```

`noeta run` never sees that test; `noeta test` compiles and runs it. Every tier works the same way.

| Tier | What it holds | Activated by |
|---|---|---|
| `@test` | code: assertions against the surrounding module | `noeta test`, see [Testing](Testing) |
| `@bench` | code: measured runs | `noeta bench`, see [Benchmarking](Benchmarking) |
| `@doc` | text: Markdown prose | `noeta doc`, see [Documentation](Documentation-and-Tiers) |
| `@debug` | code: conditional instrumentation | `--tier debug`, or a [build target](#naming-tiers-and-build-targets--noetatoml) |

## The tier model

A **tier** is a kind of co-located content, so it is a property of the source. The built-in tiers are `test`, `bench` and `debug`, which hold code, and `doc`, which holds text. Std declares all four through the same registration surface a third-party package uses, and the name-space is open, so a package can declare tiers of its own (see [Extending Tiers](Extending-Tiers)).

A **target** is a named build recipe in `noeta.toml`, so it is a property of the build invocation. It decides which tiers are live: a `development` target includes them, a `production` target strips them all.

### How activation works

On a normal `noeta run` the active-tier set is empty, and every `@<tier> { … }` block is **stripped before lowering**. The block reaches neither the type checker nor either backend, so a production build cannot carry tier content.

When a tier *is* active, its block's items are **inlined** into the top-level program, the block itself being grouping sugar, and the lifted functions gain white-box access to private fields (see [Testing](Testing)). A block is resolved wherever it appears, top-level and nested inside a function body, loop or branch alike.

Each tool activates its own tier: `noeta test` activates `test`, `noeta bench` activates `bench`, `noeta doc` activates `doc`. The `debug` tier has no dedicated command, so you activate it explicitly.

### Checking is not building

A stripped block is still source you wrote, which makes stripping a *build* decision rather than a *checking* one. So [`noeta check`](The-CLI#noeta-check) checks a file once as it ships, with every block stripped, and then **once per code tier its own blocks name**. Each of those passes is the shape `noeta test`, `noeta bench` or `noeta <tier>` compiles.

```console
$ noeta check .
checked 3 files (tiers: debug, test): 0 error(s), 0 warning(s)
```

The `(tiers: …)` clause names what it looked inside, so a green `noeta check` means the tier bodies compile too. It needs no `--target`, and a green check guarantees the `noeta test` that follows compiles.

One tier per pass. No build compiles `@test` and `@bench` blocks together, and a joint check would report two same-named helpers in two blocks as a conflict when the program is legal.

Three kinds of block add no pass, having nothing extra to type-check: a **text** tier (`@doc`, and any `text: "<lang>"` tier, whose body is verbatim text rather than Noeta), an **expression** tier, and a block written by a *dependency* rather than by this file.

### A block's own imports

A **top-level** tier block may open with its own `use`s, so a dependency only the tier needs is written where the tier is rather than at the top of the file:

```noeta
@test {
    use std.test.{Skip}

    #[Skip("needs an argument")]
    fn f(text: string): string { return text }
}
```

Such an import binds **inside the block only**. The same name used outside it resolves as if the `use` were not there, and the import is dropped with the block when the tier is inactive, so it never reaches a production build.

Everything the block's own code names through it works as a top-level `use` does, including the attribute names that `#[…]` resolves. A block in *statement* position, nested in a function body, loop or branch, is code rather than a file scope: a `use` inside one binds nothing, and its references are the ordinary "cannot find … in this scope" error.

That holds for **any** module, not just `std`: a sibling module or a dependency package is imported the same way, in either spelling, and the module it names is linked into the program just as a top-level `use` would link it.

```noeta ignore
@test {
    use probe.lib.side.{Thing}      // or: use probe.lib.side, then `side.Thing`

    fn builds(): void { assert(Thing { n: 3 }.n == 3) }
}
```

An import naming a module that does not exist is a link error (`E0019`) reported where it is written, whether or not the tier is active, so a typo surfaces at `noeta check` rather than as a pile of unresolved names the first time you run `noeta test`.

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

`@<tier> fn …` is a one-item block, sugar for wrapping a single function:

```noeta
@test fn adds(): void { assert(add(1, 2) == 3) }
```

The two forms activate, lower and run identically. They differ in checking: an annotation *attaches* to the declaration it wraps, so it is checked against the tier's declared attachment sites (**E0054**), while a block groups whatever is inside it and carries no attachment.

`noeta fmt` prints back whichever form you wrote. It canonicalizes an annotation to the directive on its own line above the declaration, and it leaves a braced block as a block.

### Directive arguments and diagnostics

A tier directive can take arguments, named as in `@bench(iterations: 1000)` or positional as in `@bench(1000)`.

A tier's knobs are declared as a prelude **config attribute**, and `bench`'s is `Bench { iterations: int }`. The directive arguments distribute that attribute: the block stamps `#[Bench(iterations: 1000)]` onto each contained fn that does not already carry one, so a per-fn attribute wins.

Validation is therefore the ordinary attribute construction gate. An unknown parameter, a duplicate, or a wrong type reports the same construction diagnostics (`E0005`/`E0007`/`E0009`) as any `#[…]` attribute, and the knobs are reflectable through `attributes_of`.

| Code | Meaning |
|---|---|
| **E0036** UnknownDirective | `@<name>` resolves to nothing in the directive name-space: not a built-in directive, not a tier, and not one any installed extension declares (`@tset`, say). Raised whether or not it would be active, so a typo never silently vanishes, and it offers the nearest real name. |
| **E0037** InvalidDirectiveArgument | Arguments on a tier with no config attribute (`@test(x)`, since `test` takes no arguments). |

---

## Naming tiers and build targets — `noeta.toml`

Two separate axes. **Which provider supplies each `@name` your source writes** is the `[directives]` table, mapping a local `@name` to `"provider[:exported]"`. One table covers directives and tiers alike, because source cannot tell them apart until resolution.

Every tier a package uses is named here, `test`, `bench`, `doc` and `debug` included: they are ordinary `std` tiers, written like any other provider's. The `:exported` half renames one, which is how two providers' same-named tiers coexist.

```toml
[dependencies]
criterion = { version = "^1.0", package = "acme/criterion" }

[directives]
test  = "std"
bench = "std"
debug = "std"
crit  = "criterion:bench"   # a dependency's `bench` tier, named `@crit` locally so it does not
                            # collide with std's `@bench`
```

**Which of those tiers are *live* in a build** is a named target's `tiers`, an activation live-set of your local tier names written as an array on the target. A bare name turns a tier on, and a `-name` turns one off, which is how a target drops a tier its `extends` base left live.

The live-set names tiers, never providers. A tier's provider is package-level, declared once in `[directives]` and the same in every build.

```toml
[targets.dev]
tiers = ["test", "debug"]

[targets.ci]
extends = "dev"
tiers = ["bench", "-debug"]   # add bench, and drop the inherited debug
```

A boolean sub-table spells the same thing, so `["bench", "-debug"]` is exactly `{bench = true, debug = false}`:

```toml
[targets.ci.tiers]
bench = true          # add bench…
debug = false         # …and drop the inherited debug
```

- A target's **active tiers** are the local names its (inheritance-merged) live-set marks `true`.
- `extends = "<base>"` inherits another target's live-set, and a nearer entry overrides the base's, so a `false` turns an inherited tier off. Cycles are detected and rejected.
- `--target <NAME>` on `noeta run` activates those tiers, unioned with `--tier`. On `noeta test`, `noeta bench` and `noeta doc`, `--target` acts as a **gate**: the tool no-ops if the target does not make its tier live.

`noeta init` scaffolds exactly this shape: a `[directives]` table naming the four std tiers, a `development` target switching them on, and an explicit `[targets.production]` with no tiers live, which gives CI and release builds a stable label.

### Target-scoped dependencies

A target can also carry its own dependencies, which **layer on top of** the global `[dependencies]` under the same overlay rule as tiers. A dev-only tool therefore reaches only the builds whose target names it.

```toml
[dependencies]                          # the default/base — present in every build
http = { version = "^1.0", package = "acme/http" }

[targets.dev.dependencies]              # layered on only when this target is selected
lint = { version = "^0.3", package = "acme/lint" }
```

The **global config is the default**. Omit `--target` and a command sees `[dependencies]` and no tiers, which is the minimal baseline. Shipping dependencies belong in the global config, and dev-only tools and tiers belong in a `[targets.dev]` overlay reached with `--target dev`. Each target contains exactly what you put in it, and the default build carries nothing that is scoped under one.

A **shipped artifact is therefore safe by default**. `noeta build` with no `--target` produces the global baseline build, and that artifact links only runtime code, as [The CLI](The-CLI#shipped-artifacts-are-lean-by-construction) covers. `--target dev` layers the dev tiers and dependencies back in when you want them.

---

## The decorator directives

Four **decorator directives**, `@derive`, `@attribute`, `@role` and `@semantic`, annotate *declarations* rather than gating content, which makes them a separate thing from the `@<tier>` blocks above. They are language features, covered in [Attributes & Reflection](Attributes-and-Reflection).

## See also

- [Testing](Testing) · [Benchmarking](Benchmarking) — the runnable tiers.
- [Documentation](Documentation-and-Tiers) — the `@doc` tier and `noeta doc`.
- [Extending Tiers](Extending-Tiers) — declaring your own tier, overriding a provider, expression tiers.
- [The `noeta` CLI](The-CLI) — the commands that drive tiers.
