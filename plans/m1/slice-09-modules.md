# Slice M1.9 — Modules / namespaces / `use` resolution

Status: in progress (M1.9.1 done — multi-file loader + linker + real `use` resolution, both backends; M1.9.2 partly done — `pub` visibility + E0019 import errors landed, E0013/SourceMap/cross-module-checking todo; M1.9.3 salsa module graph todo)

## Approach (decided with the user)

**Multi-file modules (faithful to the design doc's file = module model).** Each `.lang` file is a module declaring `namespace App.Models;`. A program is rooted at an *entry* file; its sibling `.lang` files are candidate modules. The entry's `use App.Models.{User}` resolves against modules' *declared* namespaces (not a path convention), and the new `lang-loader` crate **merges each resolved declaration into one `Program`** ahead of the entry's statements — so both backends run the linked program unchanged and the differential oracle is preserved by construction (no module-aware runtime). The conformance harness gained multi-file fixtures: a directory containing `main.lang` is one case (its siblings are that program's modules, not standalone cases).

## M1.9.1 — done

- [x] **`lang-loader` crate.** `load(entry_path)` reads the entry + its sibling `.lang` files, parses each (own `Source`/`SourceId`), builds a namespace→module map, resolves the entry's `use` declarations to real declarations, and returns one merged `Program` (`Linked`). `link(entry_name, entry_text, &[RawModule])` is the in-memory testable core. **Backward-compatible:** a `use` no loaded module provides is left in place, so the runtime keeps its M0 opaque-stub fallback — a lone file links to exactly itself, and the whole existing corpus is unaffected. A resolved name's `use` is trimmed so no opaque stub shadows the real declaration.
- [x] **CLI + harness wiring.** `lang run <file>` loads + links via the loader. The conformance harness discovers a `main.lang`-containing directory as a single multi-file case (siblings are modules, not cases) and runs the merged program through check + the tree-walker; the differential runs the merged program through **both** backends directly (the single-`Source` salsa graph can't express the link yet — that's M1.9.3), proving the VM reproduces the linked program.
- [x] **Conformance:** `modules/cross_module/` (`main.lang` `use`s `App.Models.User` from `models.lang`; the real class's constructor *and* a `greeting()` method run — an opaque stub has neither). `modules/namespace_and_use.lang` still passes on the opaque-stub fallback (single file, no sibling modules). Loader unit tests (resolve, opaque fallback, entry parse-error sourcing). Suite **67 passed**; differential **63 matched / 0 skipped / 100% / zero divergence**.

## M1.9.2 — in progress (visibility done)

- [x] **`pub` visibility (module-private by default).** New `pub` keyword (lexer `PubKw`); `is_public` on class/record/enum/fn decls (AST + parser — `pub` is parsed after any decorators, before the keyword; `pretty` renders `pub ` only when set, so non-`pub` snapshots are byte-identical). The linker now resolves *strictly* once a namespace is found: only a `pub` declaration is importable. Importing a module-private declaration or one the module does not declare is the new **E0019 `UnresolvedImport`** (a private import and a typo'd one give distinct messages, both E0019, on the imported name's span). A `use` whose namespace **no** module provides still falls back to the opaque stub (so single-file `namespace_and_use.lang` is unaffected). Conformance `modules/private_import/` (E0019 at 7:16). Loader unit tests (`importing_a_private_declaration_is_e0019`, `importing_a_missing_export_is_e0019`); parser `parses_pub_visibility`. The `modules/cross_module` `User` is now `pub`. Suite **68 passed**; differential unchanged (a load error is excluded from the differential like a parse failure). No `unsafe` touched.
- [ ] Unknown-type **E0013**: now that resolution exists, an annotation naming neither a declared/imported nor built-in type can be flagged. Needs the checker to know the linked program's resolved type names (and to not fire on still-gradual cases).
- [ ] A `SourceMap` so *check/runtime* diagnostics that land on a merged-in declaration render against the right module source (today they render against the entry source; positive linked programs produce none, so this is latent).
- [ ] Cross-module type checking over the resolved graph; a name-collision/ambiguity negative case.

## M1.9.3 — todo (see checklist below)

## Goal
Replace M0's opaque `use`-stubs with real module loading and name resolution, expressed as salsa queries so incrementality and HMR blast-radius fall out for free.

## Scope
- In: `namespace` declarations as real module identity; `use App.Models.{User}` resolving to actual declarations; a module graph as salsa queries (module boundaries = incremental-recompilation units); name resolution as a query; visibility rules (at least public/module-private). Cross-module type checking via the M1.7 checker over the resolved graph.
- Out: the package manager / external registry (M2); WASM/native extension loading (M2/M3); editions (M3).

## Checklist (vertical slice)
- [ ] Grammar / AST: none new (reuses M0 `Namespace`/`Use`); semantics change from stub to real.
- [ ] Checker rule: name resolution + visibility as salsa queries; cross-module type checking.
- [ ] Bytecode: module-qualified symbol resolution in lowering.
- [ ] VM op: module value loading (mostly compile-time resolved).
- [ ] Conformance cases: multi-module program (declare in one namespace, `use` from another), a visibility-violation negative case, a name-collision/ambiguity case.
- [ ] Snapshots: rendered diagnostics for unresolved-import / visibility errors.

## Definition of done
- The M0 `modules/namespace_and_use.lang` case runs with real resolution (no opaque stub); new multi-module cases pass.
- Module graph is salsa-queried (changing one module recomputes only dependents).
- fmt/clippy clean.

## Notes / traps
- Module boundaries must be visible to salsa so incremental recompilation knows the blast radius — this is the M2 HMR foundation, get the granularity right.
- M0 represented imported types as opaque constructable stubs; real resolution must subsume those cases without regressing the §14 demo.
