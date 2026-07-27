# noeta-ast

The abstract syntax tree: pure data, no behavior.

- **Takes in:** nothing (consumes only `noeta-span`)
- **Emits:** AST node types (every node carries a `Span`), the `SyntaxKind` tag set, and a stable `Pretty` printer for snapshots.

## Shared derivations over the AST

Being the bottom of the stack, this is also where a few **pure derivations** live once, so the crates above cannot each grow their own: `derive` (the shared derive planner), `reflect` (the reflection manifest), `desugar`, and `shape`.

`shape` answers "what is this declaration made of?" as `(member name, declared type spelling)` pairs, in declaration order — a `struct`'s or `class`'s fields, an `enum`'s variants with their payload spellings, and nothing for a declaration with no typed members. Two seams hand that answer to a native extension: the checker's `ExtDerive::validate` and the loader's `DirectiveCtx::fields` for an expanding directive. Both read the one walk, so a derive recipe and an expansion hook in the same extension can never see the same declaration differently. Spellings are the *declared surface* ones at full fidelity — `List<int>`, `?User`, no lattice normalization — with namespace-qualified identities shortened back to the name the author wrote, since the linker has already qualified them by the time either consumer runs.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
