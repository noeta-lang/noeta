# noeta-ast

The abstract syntax tree: pure data, no behavior.

- **Takes in:** nothing (consumes only `noeta-span`, plus `noeta-ext-abi` for `native_reflect` below — a leaf crate with no `noeta-*` dependencies of its own)
- **Emits:** AST node types (every node carries a `Span`), the `SyntaxKind` tag set, a stable `Pretty` printer for snapshots, and `normalize` — the span eraser that makes structural equality answerable.

## Shared derivations over the AST

Being the bottom of the stack, this is also where a few **pure derivations** live once, so the crates above cannot each grow their own: `derive` (the shared derive planner), `reflect` (the reflection manifest), `desugar`, `shape`, and `conversion`.

`conversion` names a declared conversion's body. A type may carry one `impl From<Source>` block per source and a method table has one slot per name, so a type declaring several names each conversion after the source it converts and a type declaring one leaves it under the plain `from`. Everything that builds or resolves a method table asks that one function — the checker's signature registration, IR lowering, the bytecode compiler's prototype reservation, and the reflection manifest — so the four agree by construction rather than by four walks written to match.

`shape` answers "what is this declaration made of?" as `(member name, declared type spelling)` pairs, in declaration order — a `struct`'s or `class`'s fields, an `enum`'s variants with their payload spellings, and nothing for a declaration with no typed members. Two seams hand that answer to a native extension: the checker's `ExtDerive::validate` and the loader's `DirectiveCtx::fields` for an expanding directive. Both read the one walk, so a derive recipe and an expansion hook in the same extension can never see the same declaration differently. Spellings are the *declared surface* ones at full fidelity — `List<int>`, `?User`, no lattice normalization — with namespace-qualified identities shortened back to the name the author wrote, since the linker has already qualified them by the time either consumer runs.

## Span normalization

Every node carries a `Span`, and `PartialEq` is derived on every node — so `a == b` asks "same program?" *and* "written at the same byte offsets?" at once. `normalize::zero_spans` separates them: it sets every span to a fixed value, after which the derived comparison is exactly structural equality. That is what makes `noeta fmt`'s safety gate a property rather than a proxy for one; the module's own docs carry the argument.

The shape is chosen so the walk cannot fall behind the AST. A private `Normalize` trait is implemented for `Span` (zero it), for the containers (recurse), and for the leaves (no-op), and every node's impl destructures **by name with no `..`** — so a field added tomorrow is a compile error at the one site that must consider it, and what to do with it is then decided by its type rather than by whoever added it. The leaf impls are listed explicitly, because a blanket `impl<T> Normalize for T` would silently swallow a new *node* type that does hold spans.

## Native reflection

`native_reflect` is the other half of `reflect`: what a `ReflectionInfo` answers about a declaration that lives in the **extension registry** rather than in the program's AST. `reflect::build` walks a *program*, so a native enum, class, attribute or callable is absent from the artifact however real it is to the rest of the language — and "a native class is indistinguishable from a `.noe` class" is an invariant reflection has to keep too.

It resolves **lazily**, on the lookup, and it is the ONE place that does. `ReflectionInfo`'s three table lookups (`type_named`, `params_for`, `returns_for`) consult the program's own records first and fall through to `native_reflect` on a miss, which resolves the single name it was asked about out of the `&'static` registry and memoizes that one answer. The predecessor materialized the whole registry into every compiled artifact at compile time (~400 `ParamRecord`s): 1.83M instructions on `noeta run` of a one-line program — a third of the process — and 4.5 KB in every `.noeb`. Program-first ordering is the shadowing rule; the resolution order within the native half is the eager seeding's push order, because the artifact's lookups were `find`s over a `Vec` and the first record for a name won.

Living here rather than behind an install-time hook is what makes it unconditional: a `.noeb` bundle run has no compile to arm a hook from, and would otherwise have lost native reflection entirely.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
