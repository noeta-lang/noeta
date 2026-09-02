# Extending Tiers

The tier name-space is **open**: a package declares tiers of its own through the same `ExtTier` surface and the same registry-dispatched runner seam std uses for `@test`, `@bench`, `@doc` and `@debug`. For the tier model itself (activation, stripping, `noeta.toml` build targets), see [Dev Tiers](Dev-Tiers).

## Declaring your own tier

A package, or the program itself, brings a new tier into existence with a `@tier` declaration on its **runner** function:

```noeta
// `tiers.noe` in the package `acme/fuzzkit` → the module `fuzzkit.tiers`

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

`@tier(name, …)` names the tier consumers write as `@<name> { … }`. What the decorated function has to be depends on the declaration's other arguments:

| Declaration | The decorated fn | The blocks it enables |
|---|---|---|
| `@tier(name)` | `fn(roots: List<TierRoot>): void` | code blocks, run by `noeta <name> <file>` |
| `@tier(name, config: T)` | the same runner; `T` is an `@attribute` struct | `@name(cases: 500) { … }`, stamping `#[T(cases: 500)]` on each contained fn |
| `@tier(name, text: "lang")` | `fn(roots: List<TierText>): void` | verbatim prose bodies in the named language |
| `@tier(name, expr: T)` | a **handler**, `fn(statics: List<string>, holes: List<() -> U>): T` | [expression blocks](#expression-tiers--embedded-languages-as-values) of type `T` |

A root is a prelude struct: `TierRoot { name: string, run: () -> void }` is one activated fn as a first-class handle, and `TierText { target: string, text: string }` is one verbatim body plus the declaration it sits above (`""` for module or section prose).

A `config:` distributes exactly as `bench`'s `Bench { iterations }` does: the block stamps the attribute onto each contained fn that does not already carry one, and the ordinary attribute-construction gate validates the arguments. It combines with neither `text:` nor `expr:`, both of which describe bodies that hold no fns to configure.

**E0051** is every malformed declaration, reported at the declaration: a `config:` that does not name an `@attribute` struct, an empty `text:`, `config:` paired with `text:` or `expr:`, a signature that does not match the table above, and two declarations of one tier within a single package. Redeclaring a name std or an extension already provides is legal, and is how a package offers its own `bench`; see [Choosing a tier's provider](#choosing-a-tiers-provider).

Running the tier is `noeta <name> <file>`. The unknown subcommand resolves against the tiers the file's linked program declares and dispatches to the runner **in-process**, after the toolchain compose and before the `noeta-<cmd>` external-binary probe. Roots keep white-box access and strip from a normal build, exactly like the built-in tiers.

In `noeta.toml`, any identifier is a valid local tier name in a target's `tiers` live-set. Whether it resolves, against your `[directives]` bindings, is checked where the tier is used.

### Native runners

A runner can also be written in Rust. An extension ships one through **`Extension::tier_runners`**, which returns `ExtTierRunner { tier, run }` pairs: a `&'static str` naming the `ExtTier` it drives, and a `fn(&mut dyn CommandCtx, &TierRun) -> u8` invoked with the collected roots. The runner attaches to the tier of that name declared by a unit sharing the runner's `root`, so a rename or a rescope resolves to the right one. An extension that both declares a tier and ships its runner lists it in `tiers` *and* `tier_runners`, the two being separate registration lists.

`@test`, `@bench` and `@doc` are dispatched through this same seam, so a third-party tier reaches the CLI the way std's do. A tier with no runner registers none: an inline-only tier like `debug`, or an [expression tier](#expression-tiers--embedded-languages-as-values) that uses `ExtTier::handler` instead.

### Choosing a tier's provider

A tier's provider is bound **per package** in the `[directives]` table, the same in every build. `[directives]` is where *every* `@name` a package writes is bound, directives included, because source cannot tell a directive from a tier until resolution.

```toml
[dependencies]
fuzzkit = { version = "^1.0", package = "acme/fuzzkit" }

[directives]
bench = "acme/fuzzkit"      # this package's `@bench` is fuzzkit's `@tier(bench, config: …)`
```

`noeta bench app.noe` then runs **fuzzkit's** tier: its config attribute is what `@bench(…)` blocks stamp, and its runner receives the roots. That holds whether or not a target is passed, since a target selects which tiers are *live* and never which provider a tier resolves to.

A tier a **dependency** declares is reachable only through such a binding. Importing the runner links the declaration into the program but does not enable the name, so `@fuzz { … }` without the binding is **E0036**, exactly as a dependency's directive is. A package's own `@tier` needs no binding, having no provider to choose.

To keep std's `bench` *and* fuzzkit's side by side, rename one: `crit = "fuzzkit:bench"`, written `@crit`. Two providers may export one tier name because the binding disambiguates. A provider that declares no such tier is an error naming both sides.

`doc` binds the same way, and a `doc = "docgen"` provider is the documentation-site seam: activation stamps every attached block as `#[Doc]`, and the runner walks `attributes_of::<Doc>()`.

A provider is a **package identity** (`acme/fuzzkit`), the built-in `"std"`, or a `[dependencies]` key. Prefer the identity: a key bound to a *scope* covers several member packages at once (`para` → `para/aether` + `para/db`) and cannot say which one you meant. The bindings are part of the compile and of the startup-cache key, and `noeta <tier> <file>` dispatches through them.

### The tier, or the command?

This binding retargets the **tier**: what `@bench` means, which config attribute its blocks stamp, and which runner receives the roots. The verb you type is a separate, replaceable thing. `noeta test`/`bench`/`doc` are commands `std` contributes, and a [`[trust.commands]`](Manifest#trustcommands--contributed-subcommands) binding replaces one wholesale, flags and help included.

| The package… | Bind it in |
|---|---|
| runs your existing `@test` blocks its own way (a better runner, different reporting) | `[trust.commands]` |
| defines its own block semantics or config attribute (`@fuzz(cases: 500)`) | `[directives]` |
| does both — its own tier *and* the verb that drives it | both, under the same name |

`[directives]` is part of the compile, so it changes what [`noeta check`](The-CLI#noeta-check) verifies inside those blocks. `[trust.commands]` changes only who is invoked.

---

## Expression tiers — embedded languages as values

A tier declared with **`expr: Type`** turns its blocks into *expressions*. The body is verbatim foreign-language text with **`${…}` holes**, and each block evaluates to a typed value by calling the decorated fn, the tier's **handler**, with the body's pieces:

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

`@greet { hello ${name}! }` desugars to `render(["hello ", "! "], [fn() => name])`, an ordinary call, which is where the guarantees come from:

| Guarantee | What it means |
|---|---|
| **Holes are real expressions** | They parse with the full grammar, close over the enclosing scope, and type-check against the handler's declared hole type `U`, so a mismatched `${…}` is an ordinary type error pointing *inside* the block. `${…}` follows string interpolation's contract exactly: the same `\$` escape for a literal `$`, and the text escapes `\{ \} \\` from text tiers apply too. |
| **Statics always number holes + 1** | Empty where holes touch, so the handler can interleave deterministically. |
| **Holes are thunks** | Each desugars to a zero-param closure, so *whether and when* a hole evaluates is the handler's choice: call each once for an eager DSL, skip unused fragments, or wrap them in `computed`s for a reactive template. |
| **The block's type is the handler's return type** | It must match the declared `expr:`, and is E0051 otherwise, like any broken tier declaration. |

`text: "<lang>"` is optional on an expression tier and worth setting, because the language ID drives editor highlighting of the body. An expression tier has no runner semantics: its blocks never activate or strip, and `noeta <tier>` rejects it. **E0052** covers the two ways a block lands in the wrong position: an expression tier in statement position, with its value silently discarded, and a non-`expr:` tier in expression position.

A **pure-Noeta package** can therefore ship `@sql`, `@json` or `@html` as parsed, checked, typed embedded languages, with no native code and no compiler plugin. A consumer binds it in `[directives]` (`sql = "para/db"`) and writes blocks with no import, the binding alone pulling the handler into the program, exactly as for a native provider's tier. `examples/sql_tier.noe` is a small end-to-end DSL.

### Native (Rust-package) expression tiers

A **native** package declares an expression tier the way it registers modules and types, through the extension ABI (`ExtTier`), naming the body language, the value type, and a **native handler** (a module function). A consumer binds it in `[directives]` like any other, and its blocks are then checked and typed like any expression. std uses this seam for **`@json`**: a native handler (`std.template.render`) that interleaves the statics with JSON-quoted holes.

```noeta
id = "u-7"
name = "Ada Lovelace"
row = @json { {"id": ${id}, "name": ${name}} }   // a checked `string`
echo row                                          // {"id": "u-7", "name": "Ada Lovelace"}
```

The handler receives the hole thunks as closures and invokes them through the higher-order native capability, so a native tier can be as lazy as a Noeta one. Both handler kinds are the same thing underneath, a function value the block's desugared call targets, so a native and a program-declared tier are indistinguishable to the checker, both backends, and the LSP.

### Editor support — language, highlighting, and the LSP

A tier's `text:` **is** the body's language, declared once and picked up by every consumer. It reaches the tooling three ways.

**The LSP reports it.** Hovering an embedded block's tier name (`@sql { … }`) shows `expression tier @sql — sql body, evaluates to Query`, the declared language and the value type, read from the tier registry. The registry unions the program's own `@tier` declarations with any an installed extension contributes, so a program-declared tier and a native package's tier hover identically. The block itself already hovers as its value type, `Query`, like any expression.

**Highlighting is extension-provided (VS Code / TextMate).** A package ships a TextMate injection grammar that colors its body as the foreign language, contributed with `injectTo: ["source.noeta"]`. It attaches by textual match (`injectionSelector: L:source.noeta`), so it needs no change to Noeta's own grammar, which is what lets an extension provide it. `${…}` holes are scoped back to `source.noeta` and highlight as ordinary Noeta inside the foreign text, the same split the compiler makes between foreign-language statics and checked Noeta holes. See `editors/sql-tier.tmLanguage.json` in the [para-db repo](https://github.com/noeta-lang/para-db) for the shape.

For tiers that ship no grammar of their own, the VS Code extension **generates** a per-project injection grammar: on activation and on `.noe` change it scans the workspace's `@tier(…, text: "lang")` declarations and regenerates `syntaxes/generated-tiers.tmLanguage.json`, so a project-declared tier highlights without any hand-written grammar.

#### tree-sitter overlays

**tree-sitter** highlighting of third-party tiers needs a per-project generated grammar, because a *static* grammar cannot read the declaration set to know which `@name` opens a verbatim body. The static grammar ships the `@doc` → markdown injection as its fallback.

`noeta grammar tree-sitter --out <dir>` generates the overlay for a project's declared tiers, sourced from the compiler's own tier scan plus installed native tiers. It writes `project-tiers.json` (the verbatim-body tier-name token list the grammar reads, so `@spec { … }` parses as prose) and regenerates `queries/injections.scm`, one language rule per tier. `tree-sitter generate`, or `--generate`, then rebuilds the parser. Drop the overlay into a `tree-sitter-noeta` checkout your editor points at.

## See also

- [Dev Tiers](Dev-Tiers) — the tier model, activation, and `noeta.toml` build targets.
- [Testing](Testing) · [Benchmarking](Benchmarking) — the built-in runnable tiers.
- [Native Extensions](Native-Extensions) — the extension ABI (`ExtTier`) native tiers register through.
- [Attributes & Reflection](Attributes-and-Reflection) — `@attribute` config structs and `attributes_of`.
