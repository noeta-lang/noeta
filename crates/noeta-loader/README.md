# lang-loader

Multi-file module loading and linking (M1.9).

- **Takes in:** an *entry* `.lang` file path (or, via [`link`], in-memory sources).
- **Emits:** a `Linked` — one merged [`Program`](../lang-ast) ready to type-check and run — or the entry's load-time (lex/parse) diagnostics, each paired with the source it renders against.

## The model

A program is rooted at an entry file. The other `.lang` files in the entry's directory are candidate **modules**, each declaring its identity with `namespace App.Models;`. The entry's imports —

```
use App.Models.User;
use App.Billing.{Invoice, Receipt};
```

— resolve against those declared namespaces: each imported name's *real* declaration (a class, struct, enum, or function) is pulled from the providing module and **merged into a single `Program`** ahead of the entry's own statements. Both backends then run the merged program unchanged, so the differential oracle is preserved by construction — there is no module-aware runtime, only one linked program.

## Backward-compatible by construction

Linking is purely additive. A `use` that **no** loaded module provides is left in place, so the runtime falls back to its M0 *opaque-stub* behavior (an imported name with an unknown shape that literals still construct). A single file with no sibling modules therefore links to exactly itself, and the whole existing single-file corpus is unaffected — real resolution lights up only when a sibling module actually provides the imported name, in which case the `use` is trimmed (so no duplicate opaque stub shadows the real declaration).

## Diagnostics

Each module keeps its own `Source` (the entry is `SourceId(0)`, siblings follow), so a module's lex/parse diagnostics render against that module. Visibility (`pub`, `E0019`), name-collision (`E0020`), and unknown-type (`E0013`) checks run across the merged program, and the module graph is expressed as salsa queries (`lang-db`'s `Workspace`/`linked`/`linked_checked`/`linked_bytecode`, so editing one module recomputes only its dependents). One piece remains latent: attributing a *check/runtime* diagnostic that lands on a merged-in declaration back to that declaration's own source (the merged-body `SourceMap`) — a deferred follow-up.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
