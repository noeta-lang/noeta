# Documentation — `@doc` and doc generation

A `@doc { … }` block holds extractable prose that lives with the code it documents.

`@doc` is one of the four [dev tiers](Dev-Tiers), so its content is co-located with your code and stripped from a normal build. Production carries no doc text.

This page covers the `@doc` block and everything `noeta doc` generates from it. For the tier model (activation, `noeta.toml` build targets) see [Dev Tiers](Dev-Tiers); for declaring tiers of your own, [Extending Tiers](Extending-Tiers).

---

## `@doc` — prose that lives with the code

A `@doc { … }` block holds verbatim Markdown. Its body is captured un-parsed by the lexer, so Markdown punctuation such as `#`, `*`, backticks and `%` never lexes as language tokens. The surrounding program compiles and runs normally.

```noeta
@doc {
    # Adder
}

@doc {
    `add(a, b)` returns **a + b**. Pure; no side effects.
}
fn add(a: int, b: int): int { return a + b }
```

The first block stands alone and is the file's **module doc**; the second leads `fn add` and documents it.

### Attachment — docs belong to declarations

A `@doc` block **attaches by adjacency**. Written immediately above a declaration (`fn`/`struct`/`class`/`enum`/`trait`), it documents that declaration. Blank lines between the two keep the attachment; any statement in between breaks it.

A block that attaches to nothing (above the `use` header, between sections, or standing alone) is the **module doc** if it is the file's first such block, and free-floating section prose otherwise. Position decides, and there is no new syntax to learn.

A **method** is documented the same way, with the block leading the method inside the body it is declared in:

```noeta ignore
class Users {
    @doc { List every user, newest first. }
    fn list(): List<User> { … }
}
```

This works in all three places a method can be written: a type's own body, an in-body `impl Trait { … }` block, and a standalone `impl Trait for Type { … }`. A field, an enum variant and a method declared in a `trait` take no `@doc`, because the grammar has no directive position on them.

Attachment feeds the whole toolchain from one resolution:

| Consumer | What attachment gives it |
|---|---|
| **Hover** (LSP) | Hovering a documented symbol, at its declaration or any call site, shows the doc prose under the type. |
| **`noeta doc`** | An attached block's source header carries the symbol. |
| **Runtime docstrings** | With the `doc` tier live (`noeta run --tier doc`), the attached block is stamped as a `#[Doc(text: "…")]` attribute on its declaration, so `attributes_of::<Doc>()` surfaces it at runtime. Reading them takes `use std.doc.Doc`. |

Runtime docstrings are opt-in, in the Python style. On a normal build nothing is stamped and the blocks strip as always.

A stamped record is keyed by the same target convention as the rest of [reflection](Attributes-and-Reflection): a bare name for a top-level `fn`, type or `trait`, and `Type.method` for a method. A method's prose therefore joins with `params_of`, `returns_of` and `attributes_of` on one key, which is how a framework reads documentation the author wrote once.

```noeta
use std.doc.Doc

@doc { Adds two ints. }
fn add(a: int, b: int): int { return a + b }

class Users {
    @doc { List every user. }
    fn list(): List<string> { return [] }
}

for d in attributes_of::<Doc>() { echo "${d.target}: ${d.value.text.trim()}" }
// --tier doc → "add: Adds two ints." then "Users.list: List every user."
```

Extract every `@doc` block to stdout, from the file named **and its sibling modules**:

```console
$ noeta doc adder.noe
<!-- adder.noe:1 -->
# Adder

<!-- adder.noe:5 · add -->
`add(a, b)` returns **a + b**. Pure; no side effects.
```

- The program is **not** type-checked or run. Docs extract from a parse alone, so you can pull docs from work-in-progress code.
- Each block is **dedented**, dropping leading and trailing blank lines and stripping common indentation.
- Each block is prefixed with an HTML-comment source header, `<!-- file:line -->`, plus `· symbol` for an attached block. That is valid Markdown and renders to nothing.
- A file with no `@doc` blocks prints a notice on stderr and exits `0`.

```text
noeta doc [OPTIONS] [PATH]
```

`PATH` (default `.`) is a file or a **directory**. A directory extracts every `.noe` beneath it. A file extracts that file and its sibling modules, because a `@doc` block belongs to the file it sits in, and linking merges declarations without the blocks beside them. This is the same workspace `--out` documents, so the two halves of `noeta doc` agree on what "the docs" means. A file that does not parse contributes nothing rather than failing the run.

### Generating a documentation artifact

`noeta doc <FILE> --out <DIR>` generates the **package documentation artifact** instead of extracting to stdout. It writes two things:

- **`docs.json`**, the canonical machine-readable form. It is schema-versioned and keyed by the package's `[package]` identity and version, and holds modules with their namespace, module doc, and items in source order, with sections woven between declarations. It is deterministic, carrying no timestamps and no absolute paths, so the artifact is content-addressable and registry-ready: a published package's docs can ride along and be rendered server-side.
- **`index.md` plus one page per module**, a faithful Markdown rendering of the same data. Each public declaration appears as a signature code block, carrying its `@tier` and `@attribute` directives, followed by its adjacency-attached prose.

Generation works from a bare parse, and a sibling that fails to parse is skipped with a note rather than being fatal.

### Docs on the registry

`noeta publish` generates the package's `docs.json` and stores it **with the release**. Skip it with `--no-docs`; a docs failure warns and never blocks a publish. Fetch any published package's docs back:

```console
$ noeta doc --package acme/greeter            # highest version — docs.json to stdout
$ noeta doc --package acme/greeter@0.3.0 --out docs/   # pinned — render the Markdown tree
```

Stored docs are *advisory metadata* rather than provenance: unsigned, last-wins on re-publish, and a hosted registry may regenerate them from source itself rather than trust the upload.

### The other flags

`noeta doc` has two further concerns: gating plain extraction on a build target, and a second mode, `--api`, which documents the **intrinsic registry** (the stdlib and any composed native modules) instead of `.noe` source. Run `noeta doc --help` for the full text.

| Flag | Applies to | Effect |
|---|---|---|
| `--target <NAME>` | `.noe` sources, extraction and `--out` alike (not `--api` or `--package`) | Gate: work only when the `doc` tier is live in that [build target](Dev-Tiers#naming-tiers-and-build-targets--noetatoml). Otherwise it reports the inactive tier and exits `0`. |
| `--api` | — | Document the intrinsic registry instead of `.noe` source. |
| `--root <NAMESPACE>` | `--api` | Scope to the extensions rooted at one namespace (excluding `std`). |
| `--non-builtin` | `--api` | Scope to every non-builtin extension. This is what `noeta publish` uses in a package's composed toolchain: it never guesses a root, so an extension whose namespace root diverges from its package name still documents. |
| `--lint` | `--api --root` / `--api --non-builtin` | The publish quality gate. Fail before emitting docs if the scoped extensions register any symbol outside their own namespace roots. Exit 2 lists the offenders. |

---

## Folding a sample's context — `// sample:start` / `// sample:end`

An untagged ` ```noeta ` block in these docs is run through the real `noeta` binary by CI and must exit `0`, so a renamed method or a changed diagnostic fails the build rather than misleading a reader. The cost is that such a sample has to be a **complete program**, and the struct, imports and helper that make a two-line call compile can bury the two lines worth reading.

Mark the interesting region and the rest folds away:

```text
struct Email { addr: string }     ← context: compiled, folded away
// sample:start
e = Email { addr: "a@b.com" }     ← the sample: what the page shows
echo e.addr
// sample:end
```

That renders as the two marked lines, with the whole program behind an expander. The block below is the live article, so open it to see the `struct` the sample needs:

```noeta
struct Email { addr: string }

// sample:start
e = Email { addr: "a@b.com" }
echo e.addr
// sample:end
```

The markers are ordinary comments, so the whole block still compiles and still runs in CI. A tag on the fence changes what CI does with it: ` ```noeta check ` type-checks a sample without executing it, ` ```noeta error ` demands a non-zero exit, and ` ```noeta ignore ` opts an illustrative fragment out. Only presentation changes:

- the **Docs browser** (VS Code) shows the marked region, with a *Show full example* expander;
- **`noeta doc --out`** and other static markdown bake the same fold in as a `<details>` block, so a reader on GitHub gets the short version with the full program one click away;
- a block with **no markers** renders in full, unchanged.

A block may mark several regions, and they concatenate in order, so a page can show two interesting stretches of one program and fold the plumbing between them. Folding keeps the context compiling, which is what keeps the sample checkable.

## See also

- [Dev Tiers](Dev-Tiers) — the tier model `@doc` belongs to: activation, stripping, `noeta.toml` build targets.
- [Extending Tiers](Extending-Tiers) — declaring your own tier (including a `doc` provider override), expression tiers.
- [The `noeta` CLI](The-CLI) — the commands that drive tiers.
