# Extending Tiers

The built-in [dev tiers](Dev-Tiers) — `@test`, `@bench`, `@doc`, `@debug` — are not special-cased in the compiler: std declares them through the same `ExtTier` surface a third-party extension uses, drives them through the same registry-dispatched runner seam, and the tier name-space is **open**. This page covers the extension points: declaring a tier of your own, binding a tier to a dependency's provider (and renaming to avoid collisions), and **expression tiers** — embedded languages as typed values. For the tier model itself (activation, stripping, `noeta.toml` build targets), see [Dev Tiers](Dev-Tiers).

## Declaring your own tier

A package (or the program itself) brings a new tier into existence with a `@tier` declaration on its **runner** function:

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

- `@tier(name[, config: Type])` names the tier consumers write as `@<name> { … }`; the optional `config:` names an `@attribute` struct whose fields are the tier's knobs — exactly the `Bench { iterations }` model, so `@fuzz(cases: 500) { … }` stamps `#[Fuzz(cases: 500)]` onto each contained fn and the ordinary construction gate validates it.
- The decorated fn is the **runner**: it must be `fn(roots: List<TierRoot>): void`, where each `TierRoot { name: string, run: () -> void }` is an activated root fn as a first-class handle. Anything else about the declaration — a name colliding with a built-in, a duplicate, a non-attribute config, a wrong signature — is **E0051** at the declaration.
- A consumer opts in with one import (`use fuzzkit.tiers.run_fuzz` — the config struct links along with the runner), writes `@fuzz { … }` blocks, and runs **`noeta fuzz <file>`**: the unknown subcommand resolves against the file's declared tiers and dispatches to the runner in-process, after the compose and before the `noeta-<cmd>` external-binary probes. Roots keep white-box access and strip from a normal build, exactly like the built-in tiers.
- In `noeta.toml`, any identifier is a valid local tier name in a target's `tiers` live-set — whether it resolves (against your `[directives]` bindings) is checked where the tier is used.

### Native runners

A runner does not have to be written in Noeta. A Rust extension can ship one through **`Extension::tier_runners`**, which returns `ExtTierRunner { tier, run }` pairs — a `&'static str` naming the `ExtTier` it drives and a `fn(&mut dyn CommandCtx, &TierRun) -> u8` invoked with the collected roots. The runner attaches to the tier of that name declared by a unit sharing the runner's `root`, so a rename or a rescope resolves to the right one. An extension that both declares a tier and ships its runner lists it in `tiers` *and* `tier_runners`; the two are separate lists because std's own declarations live beneath the CLI while its native runners live inside it, and the pair cannot be spelled in one literal.

This is not a std privilege — `@test`, `@bench`, and `@doc` are dispatched through exactly this seam, so a third-party tier reaches the CLI the same way. A tier with no runner registers none: an inline-only tier like `debug`, or an [expression tier](#expression-tiers--embedded-languages-as-values) that uses `ExtTier::handler` instead.

### Choosing a tier's provider

A tier's provider is bound **per package** in the `[directives]` table, not per target — the same in every build. `[directives]` is where *every* `@name` a package writes is bound, directives included: source cannot tell a directive from a tier until resolution, so the manifest does not ask you to. To use a dependency's tier, name it there:

```toml
[dependencies]
fuzzkit = { version = "^1.0", package = "acme/fuzzkit" }

[directives]
bench = "acme/fuzzkit"      # this package's `@bench` is fuzzkit's `@tier(bench, config: …)`
```

`noeta bench app.noe` runs **fuzzkit's** tier — its config attribute is what `@bench(…)` blocks stamp, and its runner receives the roots — whether or not a target is passed (a target only selects which tiers are *live*, never which provider a tier resolves to). A tier a **dependency** declares is reachable only through such a binding — an unbound name is not ambient. To keep std's `bench` *and* fuzzkit's side by side, rename one (`crit = "fuzzkit:bench"`, written `@crit`). Two providers may export the same tier name precisely because the binding disambiguates; `E0051` rejects only two declarations of one tier *within one package*. A provider that declares no such tier is an error naming both sides. `doc` binds the same way — a `doc = "docgen"` provider is the documentation-site seam: activation stamps every attached block as `#[Doc]`, and the runner walks `attributes_of::<Doc>()`. The `[directives]` bindings are part of the compile (and the startup-cache key), and `noeta <tier> <file>` dispatches through them.

A provider is a **package identity** (`acme/fuzzkit`), the built-in `"std"`, or a `[dependencies]` key. Prefer the identity: a key bound to a *scope* covers several member packages at once (`para` → `para/aether` + `para/db`) and cannot say which one you meant.

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

Because the declaration is ordinary code, a **pure-Noeta package** can ship `@sql`, `@json`, or `@html` — parsed, checked, typed embedded languages — with no native code and no compiler plugin. A consumer binds it in `[directives]` (`sql = "para/db"`) and writes blocks; **no import is involved**, and none is enough on its own. The binding is what pulls the handler into the program, exactly as it is for a native provider's tier — one rule for both kinds, which is the point. See `examples/sql_tier.noe` for a small end-to-end DSL.

### Native (Rust-package) expression tiers

A **native** package declares an expression tier the same way it registers modules and types — through the extension ABI (`ExtTier`), naming the body language, the value type, and a **native handler** (a module function). A consumer binds it in `[directives]` like any other, and its blocks are then checked and typed like any expression. std itself uses this seam for **`@json`**: a native handler (`std.template.render`) that interleaves the statics with JSON-quoted holes.

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
- **Highlighting is extension-provided (VS Code / TextMate).** A package ships a TextMate injection grammar that colors its body as the foreign language, contributed with `injectTo: ["source.noeta"]`. It attaches by textual match (`injectionSelector: L:source.noeta`), so it needs no change to Noeta's own grammar — that is why an extension can provide it. `${…}` holes are scoped back to `source.noeta`, so they highlight as ordinary Noeta inside the foreign text — the same split the compiler makes (statics = foreign language, holes = checked Noeta). See `editors/sql-tier.tmLanguage.json` in the [para-db repo](https://github.com/noeta-lang/para-db) for the shape. For tiers that ship no grammar of their own, the VS Code extension also **generates** a per-project injection grammar: on activation and on `.noe` change it scans the workspace's `@tier(…, text: "lang")` declarations and regenerates `syntaxes/generated-tiers.tmLanguage.json`, so a project-declared tier highlights without any hand-written grammar.
- **tree-sitter** highlighting of third-party tiers needs a per-project generated grammar (a *static* grammar cannot read the declaration set to know which `@name` opens a verbatim body). The static grammar ships the `@doc` → markdown injection as its fallback; `noeta grammar tree-sitter --out <dir>` generates the overlay for a project's declared tiers — the same model as the TextMate generator, but sourced from the compiler's own tier scan (plus installed native tiers) rather than a regex. It writes `project-tiers.json` (the verbatim-body tier-name token list the grammar reads, so `@spec { … }` parses as prose) and regenerates `queries/injections.scm` (one language rule per tier); `tree-sitter generate` (or `--generate`) then rebuilds the parser. Drop the overlay into a `tree-sitter-noeta` checkout your editor points at.

## See also

- [Dev Tiers](Dev-Tiers) — the tier model, activation, and `noeta.toml` build targets.
- [Testing](Testing) · [Benchmarking](Benchmarking) — the built-in runnable tiers.
- [Native Extensions](Native-Extensions) — the extension ABI (`ExtTier`) native tiers register through.
- [Attributes & Reflection](Attributes-and-Reflection) — `@attribute` config structs and `attributes_of`.
