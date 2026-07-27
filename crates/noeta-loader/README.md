# noeta-loader

Multi-file module loading and linking (M1.9).

- **Takes in:** an *entry* `.noe` file path (or, via [`link`], in-memory sources).
- **Emits:** a `Linked` — one merged [`Program`](../noeta-ast) ready to type-check and run — or the entry's load-time (lex/parse) diagnostics, each paired with the source it renders against.

## The model

A program is rooted at an entry file. The other `.noe` files in the entry's directory are candidate **modules**, each declaring its identity with `namespace App.Models;`. The entry's imports —

```
use App.Models.User;
use App.Billing.{Invoice, Receipt};
```

— resolve against those declared namespaces: each imported name's *real* declaration (a class, struct, enum, or function) is pulled from the providing module and **merged into a single `Program`** ahead of the entry's own statements. Both backends then run the merged program unchanged, so the differential oracle is preserved by construction — there is no module-aware runtime, only one linked program.

## Backward-compatible by construction

Linking is purely additive. A `use` that **no** loaded module provides is left in place, so the runtime falls back to its M0 *opaque-stub* behavior (an imported name with an unknown shape that literals still construct). A single file with no sibling modules therefore links to exactly itself, and the whole existing single-file corpus is unaffected — real resolution lights up only when a sibling module actually provides the imported name, in which case the `use` is trimmed (so no duplicate opaque stub shadows the real declaration).

## Compile-time directive expansion

Linking is also where an extension's `ExtDirective::expand` hook runs (`expand.rs`), because generated members have to be in the one merged `Program` before anything checks it. Every link entry point routes through the single `run_expansion`, so the editor and the compiler can never disagree about a decorated type's members. Each expansion becomes a **real `Source`** appended to the program's source map, so generated code has true spans; the files the hooks reported reading come back too, as the rebuild trigger a watcher folds into its watch set.

The hook is given a `DirectiveCtx`: the invocation (`args`, `named`), the declaration it decorates (`target`, `site`, and its members as `fields` — via the shared `noeta_ast::shape` derivation, so a hook can generate from a struct's *shape* and sees exactly what the checker hands `ExtDerive::validate`), and the directive's own directory so a relative path argument resolves against the file rather than the process's working directory. Nothing about the surrounding program, so an expansion's output depends only on inputs the caller can key a memoized result on.

## Diagnostics

Each module keeps its own `Source` (the entry is `SourceId(0)`, siblings follow), so a module's lex/parse diagnostics render against that module. Visibility (`pub`, `E0019`) and unknown-type (`E0013`) are properties of the merged program; **name collision (`E0020`) is not** — a `use` binds in one file, so the collision question is asked per *compilation unit* (the entry, or a pooled module driving its own imports). Two files importing different declarations under the same short name is exactly how two packages sharing an import root coexist, and is never a clash. The module graph is expressed as salsa queries (`noeta-db`'s `Workspace`/`linked`/`linked_checked`/`linked_bytecode`, so editing one module recomputes only its dependents).

An import diagnostic is built by the linking core, which sees `Program`s and no `Source`s, so it can only name the entry as a provisional render target; `attribute_to_spans` then re-points each one at the file its span actually indexes. One piece remains latent: attributing a *check/runtime* diagnostic that lands on a merged-in declaration back to that declaration's own source (the merged-body `SourceMap`) — a deferred follow-up.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
