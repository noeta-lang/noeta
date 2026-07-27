# Documentation — `@doc` and doc generation

A `@doc { … }` block holds extractable prose that lives with the code it documents. `@doc` is one of the four [dev tiers](Dev-Tiers) — co-located content stripped from a normal build by construction — so **production carries no doc text**. This page covers the `@doc` block itself and everything `noeta doc` generates from it. For the tier model (activation, `noeta.toml` build targets) see [Dev Tiers](Dev-Tiers); for declaring tiers of your own, [Extending Tiers](Extending-Tiers).

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
noeta doc [OPTIONS] [PATH]
```

`PATH` (default `.`) is a file or a **directory**. A directory extracts every `.noe` beneath it; a
file extracts that file **and its sibling modules**, because a `@doc` block belongs to the file it
sits in and linking merges declarations without the blocks beside them — extracting from the linked
program alone would silently drop the documentation of every imported symbol. This is the same
workspace `--out` has always documented, so the two halves of `noeta doc` agree on what "the docs"
means. A file that does not parse contributes nothing rather than failing the run.

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

### The other flags

`noeta doc` has two more concerns: gating plain extraction on a build target, and a second mode, `--api`, which documents the **intrinsic registry** (the stdlib and any composed native modules) instead of `.noe` source. See `noeta doc --help` for the full text.

| Flag | Applies to | Effect |
|---|---|---|
| `--target <NAME>` | plain extraction (`noeta doc <FILE>`, no `--out`/`--package`) | Gate: extract only when the `doc` tier is live in that [build target](Dev-Tiers#build-targets--noetatoml); otherwise nothing is emitted. |
| `--api` | — | Document the intrinsic registry instead of `.noe` source. |
| `--root <NAMESPACE>` | `--api` | Scope to the extensions rooted at one namespace (excluding `std`). |
| `--non-builtin` | `--api` | Scope to every non-builtin extension — what `noeta publish` uses in a package's composed toolchain: it never guesses a root, so an extension whose namespace root diverges from its package name still documents. |
| `--lint` | `--api --root` / `--api --non-builtin` | Fail (before emitting docs) if the scoped extensions register any symbol outside their own namespace roots — the publish quality gate. Exit 2 lists the offenders. |

---

## Folding a sample's context — `// sample:start` / `// sample:end`

Every ` ```noeta ` block in these docs is run through the real `noeta` binary by CI, which is what
keeps the documentation honest: a renamed method or a changed diagnostic fails the build instead of
quietly misleading a reader. The cost is that a sample has to be a **complete program** — and the
struct, imports and helper that make a two-line call compile can easily bury the two lines worth
reading.

Mark the interesting region and the rest folds away:

```noeta
struct Email { addr: string }

// sample:start
e = Email { addr: "a@b.com" }
echo e.addr
// sample:end
```

The markers are ordinary comments, so **the whole block still compiles and still runs in CI** —
nothing about the gate changes. Only presentation does:

- the **Docs browser** (VS Code) shows the marked region, with a *Show full example* expander;
- **`noeta doc --out`** and other static markdown bake the same fold in as a `<details>` block, so a
  reader on GitHub gets the short version with the full program one click away;
- a block with **no markers** renders exactly as it always has.

A block may mark several regions — they concatenate in order, so a page can show two interesting
stretches of one program and fold the plumbing between them. Shortening a sample this way is
strictly better than deleting the context: the deleted version stops compiling, and a sample that
does not compile is a sample nothing can check.

## See also

- [Dev Tiers](Dev-Tiers) — the tier model `@doc` belongs to: activation, stripping, `noeta.toml` build targets.
- [Extending Tiers](Extending-Tiers) — declaring your own tier (including a `doc` provider override), expression tiers.
- [The `noeta` CLI](The-CLI) — the commands that drive tiers.
