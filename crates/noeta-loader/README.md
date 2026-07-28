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

## What gets merged: imports, their closure, every impl, and every annotated declaration

Four things put a pooled module's declaration into the merged program:

1. an **import** naming it (the `use`-driven merge above);
2. the **same-module closure** of anything already merged — the internal helper an exported `fn` calls, the module-local type it names in a parameter/return/field;
3. a **standalone `impl`** whose target type is anywhere in the program (the entry's own declarations included), because an impl has no import name and must travel with its type or the type arrives without its traits;
4. a **`#[...]` data attribute** anywhere on it — on the declaration itself, or on a method, field, variant, or parameter.

Visibility does not gate an intra-module reference, so a non-`pub` helper is pulled.

The third runs to a **fixpoint**, because "is the target type in the program?" is a question whose answer grows while it is being asked: an impl's own body closure merges the declarations it names, so `impl Codec for MyCodec { fn decoder(): dyn Decoder { return MyDecoder.new() } }` is what puts `MyDecoder` in the program, and `impl Decoder for MyDecoder` only becomes eligible after it. One pass could not see that in either source order, and the impl it dropped failed silently — the type linked and its inherent methods dispatched, so only the trait went missing, in a consumer of the package and never in the package's own tests. Each impl is deduped on its **span**, the identity of a declaration being where it is written; deduping on `(target, trait)` instead named the coherence *slot*, so two modules that each implemented one trait for one type silently collapsed into whichever the scan reached first rather than reaching the checker as the E0027 they are.

The fourth is `carries_data_attribute`, and it exists because an attribute's whole purpose is to make a declaration findable by something that never names it: `attributes_of::<Tool>()` discovers it and `invoke` calls it by name. Merging only along `use` edges meant the manifest held just the annotated declarations the entry happened to import — the registration mechanism could not see its own registrations, and reflection could not report what dispatch could reach. An annotated root is merged with the same closure an imported one gets, so it also *runs*.

It is scoped to the annotation, not the file: an unannotated declaration nothing references still stays out, so this is not whole-directory compilation. A `@derive`/`@role`/`@packed` **directive** is deliberately not a root — it drives codegen on a declaration already in the program rather than registering one for discovery. (A `@role` still reaches the manifest transitively: it rides on an `@attribute` struct, and it is the *applications* of that struct that are roots.)

## Backward-compatible by construction

Linking is purely additive. A `use` that **no** loaded module provides is left in place, so the runtime falls back to its M0 *opaque-stub* behavior (an imported name with an unknown shape that literals still construct). A single file with no sibling modules therefore links to exactly itself, and the whole existing single-file corpus is unaffected — real resolution lights up only when a sibling module actually provides the imported name, in which case the `use` is trimmed (so no duplicate opaque stub shadows the real declaration).

## One flat scope, so file-scoped names are renamed into it

The merged program has **one global scope**, and that scope is the *entry's*: its own short names are already the program's. Every other unit's file-scoped names are therefore rewritten into it, in both namespaces a file binds:

- **Types and declarations** take their qualified identity (`User` → `App.Models.User`), through a module's `UnitMap::names`.
- **Native `use` handles** — the value bindings `use std.http.url` (`url`), `use std.{json}` (`json`), `use std.http.url.{decode}` (`decode`) create — take the import's **canonical identity** (`std.http.url`, `std.json`, `std.http.url.decode`), through `UnitMap::handles`. The retained `use` is aliased to the same name, so the binding both backends create and the reference that reads it are one decision, taken here.

Without that second rewrite a leaf name was the binding key across every unit: a dependency's `use std.http.url` and an unrelated package's `use para.url` both claimed the global `url`, last writer won, and the dependency called into a module it had never heard of — while the checker, which keeps its own per-import table, answered correctly. The program checked clean and failed at run time. `Registry::classify_use` is the one classifier all four consumers (checker, both backends, this linker) call, so the canonical name recorded here is by construction the identity they resolve the import to.

## Compile-time directive expansion

Linking is also where an extension's `ExtDirective::expand` hook runs (`expand.rs`), because generated members have to be in the one merged `Program` before anything checks it. Every link entry point routes through the single `run_expansion`, so the editor and the compiler can never disagree about a decorated type's members. Each expansion becomes a **real `Source`** appended to the program's source map, so generated code has true spans; the files the hooks reported reading come back too, as the rebuild trigger a watcher folds into its watch set.

The hook is given a `DirectiveCtx`: the invocation (`args`, `named`), the declaration it decorates (`target`, `site`, and its members as `fields` — via the shared `noeta_ast::shape` derivation, so a hook can generate from a struct's *shape* and sees exactly what the checker hands `ExtDerive::validate`), and the directive's own directory so a relative path argument resolves against the file rather than the process's working directory. Nothing about the surrounding program, so an expansion's output depends only on inputs the caller can key a memoized result on.

## Diagnostics

Each module keeps its own `Source` (the entry is `SourceId(0)`, siblings follow), so a module's lex/parse diagnostics render against that module. Visibility (`pub`, `E0019`) and unknown-type (`E0013`) are properties of the merged program; **name collision (`E0020`) is not** — a `use` binds in one file, so the collision question is asked per *compilation unit* (the entry, or a pooled module driving its own imports). Two files importing different declarations under the same short name is exactly how two packages sharing an import root coexist, and is never a clash. The module graph is expressed as salsa queries (`noeta-db`'s `Workspace`/`linked`/`linked_checked`/`linked_bytecode`, so editing one module recomputes only its dependents).

An import diagnostic is built by the linking core, which sees `Program`s and no `Source`s, so it can only name the entry as a provisional render target; `attribute_to_spans` then re-points each one at the file its span actually indexes. One piece remains latent: attributing a *check/runtime* diagnostic that lands on a merged-in declaration back to that declaration's own source (the merged-body `SourceMap`) — a deferred follow-up.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
