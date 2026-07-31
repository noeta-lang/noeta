# Dev Tiers

Tests, benchmarks, documentation, and debug instrumentation live *with* the code they describe — inside `@test`, `@bench`, `@doc`, and `@debug` blocks. These are **dev tiers**: kinds of co-located content that are stripped from a normal build and activated only by the right tool.

```noeta
fn add(a: int, b: int): int { return a + b }

@test fn adds(): void { assert(add(1, 2) == 3) }
```

`noeta run` never sees the test; `noeta test` brings it to life. Every tier works this way:

| Tier | What it holds | Activated by |
|---|---|---|
| `@test` | code — assertions against the surrounding module | `noeta test` — see [Testing](Testing) |
| `@bench` | code — measured runs | `noeta bench` — see [Benchmarking](Benchmarking) |
| `@doc` | text — Markdown prose | `noeta doc` — see [Documentation](Documentation-and-Tiers) |
| `@debug` | code — conditional instrumentation | no dedicated command — `--tier debug`, or a [build target](#naming-tiers-and-build-targets--noetatoml) |

## The tier model

There are two orthogonal ideas:

- A **tier** is a *kind of co-located content* — a property of the **source**. Built-in tiers: `test`, `bench`, `debug` (all *code*), and `doc` (*text*). None of them is special-cased in the compiler — std declares them through the same surface a third-party package uses, and the name-space is open: a package can declare tiers of its own (see [Extending Tiers](Extending-Tiers)).
- A **target** is a *named build recipe* — a property of the **build invocation** (in `noeta.toml`) — that decides which tiers are live: a `development` target includes them, a `production` target strips them all.

### How activation works

On a normal `noeta run`, the active-tier set is empty: every `@<tier> { … }` block is **stripped before lowering**. It never reaches the type checker or either backend, so tier content can never affect a production build — the stripping is by construction, not a dead-code pass.

When a tier *is* active, its block's items are **inlined** into the top-level program (the block is pure grouping sugar), and the lifted functions gain white-box access to private fields (see [Testing](Testing)). A block is resolved wherever it appears — top-level *and* nested inside a function body, loop, or branch.

Each tool activates its own tier: `noeta test` activates `test`, `noeta bench` activates `bench`, `noeta doc` activates `doc`. The `debug` tier has no dedicated command — you activate it explicitly.

### Checking is not building

Stripping is a *build* decision, not a *checking* one: a stripped block is still source you wrote. So [`noeta check`](The-CLI#noeta-check) checks a file once as it ships — every block stripped — and then **once per code tier its own blocks name**, which is exactly the shape `noeta test`, `noeta bench`, or `noeta <tier>` compiles.

```console
$ noeta check .
checked 3 files (tiers: test, debug): 0 error(s), 0 warning(s)
```

The `(tiers: …)` clause names what it looked inside, so a green `noeta check` means the tier bodies compile too, not that nobody looked. It needs no `--target`: a green check is never followed by a `noeta test` that fails to compile.

One tier per pass, never all at once. No build ever compiles `@test` and `@bench` blocks together, so checking them jointly would invent collisions between them — two same-named helpers in two blocks are not a conflict, and must not be reported as one.

Three kinds of block add no pass, because there is nothing extra to type-check: a **text** tier (`@doc`, and any `text: "<lang>"` tier — its body is verbatim text, not Noeta), an **expression** tier, and a block written by a *dependency* rather than by this file.

### A block's own imports

A **top-level** tier block may open with its own `use`s, so a dependency only the tier needs is written where the tier is rather than at the top of the file:

```noeta
@test {
    use std.test.{Skip}

    #[Skip("needs an argument")]
    fn f(text: string): string { return text }
}
```

Such an import binds **inside the block only** — the same name used outside it resolves to nothing, exactly as if the `use` were not there — and it is dropped with the block when the tier is inactive, so it never reaches a production build. Everything the block's own code can name through it works the same as a top-level `use`, including the attribute names that `#[…]` resolves. (A block in *statement* position — nested in a function body, loop, or branch — is code, not a file scope: a `use` inside one binds nothing and its references are the ordinary "cannot find … in this scope" error.)

That holds for **any** module, not just `std`: a sibling module or a dependency package is imported the same way, in either spelling, and the module it names is linked into the program just as a top-level `use` would link it.

```noeta ignore
@test {
    use probe.lib.side.{Thing}      // or: use probe.lib.side, then `side.Thing`

    fn builds(): void { assert(Thing { n: 3 }.n == 3) }
}
```

An import naming a module that does not exist is a link error (`E0019`) reported where it is written, whether or not the tier is active — so a typo surfaces at `noeta check`, not as a pile of unresolved names the first time you run `noeta test`.

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
| **E0036** UnknownDirective | `@<name>` resolves to nothing in the directive name-space — not a built-in directive, not a tier, and not one any installed extension declares (e.g. `@tset`). Raised whether or not it would be active, so a typo never silently vanishes, and it offers the nearest real name. |
| **E0037** InvalidDirectiveArgument | Arguments on a tier with no config attribute (`@test(x)` — `test` takes no arguments). |

---

## Naming tiers and build targets — `noeta.toml`

Two separate axes. **Which provider declares each tier your source uses** is the `[tiers]` table — a local `@name` → `"provider[:exported]"`, the tier counterpart of `[directives]`. There are no ambient built-in tiers: `test`/`bench`/`doc`/`debug` are ordinary `std` tiers you name here like any other provider's, and `:exported` renames one (to dodge a collision between two providers' same-named tiers):

```toml
[tiers]
test  = "std"
bench = "std"
debug = "std"
crit  = "criterion:bench"   # a dependency's `bench` tier, named `@crit` locally so it does not
                            # collide with std's `@bench`
```

**Which of those tiers are *live* in a build** is a named target's `tiers` — an activation live-set of your local tier names, written as an array on the target: a bare name turns a tier on, a `-name` turns one off (to drop a tier an `extends` base left live). It no longer names a provider (that moved to `[tiers]`); a tier's provider is package-level, the same in every build:

```toml
[targets.dev]
tiers = ["test", "debug"]

[targets.ci]
extends = "dev"
tiers = ["bench", "-debug"]   # add bench, and drop the inherited debug
```

The equivalent boolean sub-table is still accepted — `["bench", "-debug"]` is exactly `{bench = true, debug = false}`:

```toml
[targets.ci.tiers]
bench = true          # add bench…
debug = false         # …and drop the inherited debug
```

- A target's **active tiers** are the local names its (inheritance-merged) live-set marks `true`.
- `extends = "<base>"` inherits another target's live-set; a nearer entry overrides the base's (a `false` turns an inherited tier off). Cycles are detected and rejected.
- `--target <NAME>` on `noeta run` activates those tiers (unioned with `--tier`). On `noeta test`/`bench`/`doc`, `--target` acts as a **gate** — the tool no-ops if the target does not make its tier live.

`noeta init` scaffolds exactly this shape: a `[tiers]` table naming the four std tiers, a `development` target switching them on beside an explicit `[targets.production]` with no tiers live — a stable label for CI and release builds.

### Target-scoped dependencies

A target can also carry its own dependencies, which **layer on top of** the global `[dependencies]` — the same overlay rule as tiers, so a dev-only tool never rides into a build that didn't ask for it:

```toml
[dependencies]                          # the default/base — present in every build
http = { version = "^1.0", package = "acme/http" }

[targets.dev.dependencies]              # layered on only when this target is selected
lint = { version = "^0.3", package = "acme/noeta-lint" }
```

The **global config is the default**: omit `--target` and a command sees `[dependencies]` and no tiers — the minimal, safe baseline. Put your shipping dependencies in the global config and keep dev-only tools/tiers in a `[targets.dev]` overlay you opt into with `--target dev`. There is no separate "dev vs prod" concept baked into the language — *you* decide what each target contains; the default build simply excludes anything you scoped under a target.

This is why a **shipped artifact is safe by default**: `noeta build` with no `--target` produces the global (baseline) build, and (as [The CLI](The-CLI#shipped-artifacts-are-lean-by-construction) covers) that artifact links only runtime code — never the dev toolchain. `--target dev` layers dev tiers/deps back in when you actually want them.

---

## Related: the decorator directives

The `@<tier>` blocks above are distinct from the four **decorator directives** — `@derive`, `@attribute`, `@role`, `@semantic` — which annotate *declarations* rather than gate content. Those are language features, covered in [Attributes & Reflection](Attributes-and-Reflection).

## See also

- [Testing](Testing) · [Benchmarking](Benchmarking) — the runnable tiers.
- [Documentation](Documentation-and-Tiers) — the `@doc` tier and `noeta doc`.
- [Extending Tiers](Extending-Tiers) — declaring your own tier, overriding a provider, expression tiers.
- [The `noeta` CLI](The-CLI) — the commands that drive tiers.
