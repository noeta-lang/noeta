# Slice M1.9 — Modules / namespaces / `use` resolution

Status: in progress (M1.9.1 done — multi-file loader + linker + real `use` resolution, both backends; M1.9.2 done — `pub` visibility + E0019 import errors + E0013 unknown-type + E0020 name-collision; cross-module checking falls out of the merge; only the latent `SourceMap` (correct source attribution for merged-in-body diagnostics) is deferred; M1.9.3 salsa module graph todo)

## Approach (decided with the user)

**Multi-file modules (faithful to the design doc's file = module model).** Each `.lang` file is a module declaring `namespace App.Models;`. A program is rooted at an *entry* file; its sibling `.lang` files are candidate modules. The entry's `use App.Models.{User}` resolves against modules' *declared* namespaces (not a path convention), and the new `lang-loader` crate **merges each resolved declaration into one `Program`** ahead of the entry's statements — so both backends run the linked program unchanged and the differential oracle is preserved by construction (no module-aware runtime). The conformance harness gained multi-file fixtures: a directory containing `main.lang` is one case (its siblings are that program's modules, not standalone cases).

## M1.9.1 — done

- [x] **`lang-loader` crate.** `load(entry_path)` reads the entry + its sibling `.lang` files, parses each (own `Source`/`SourceId`), builds a namespace→module map, resolves the entry's `use` declarations to real declarations, and returns one merged `Program` (`Linked`). `link(entry_name, entry_text, &[RawModule])` is the in-memory testable core. **Backward-compatible:** a `use` no loaded module provides is left in place, so the runtime keeps its M0 opaque-stub fallback — a lone file links to exactly itself, and the whole existing corpus is unaffected. A resolved name's `use` is trimmed so no opaque stub shadows the real declaration.
- [x] **CLI + harness wiring.** `lang run <file>` loads + links via the loader. The conformance harness discovers a `main.lang`-containing directory as a single multi-file case (siblings are modules, not cases) and runs the merged program through check + the tree-walker; the differential runs the merged program through **both** backends directly (the single-`Source` salsa graph can't express the link yet — that's M1.9.3), proving the VM reproduces the linked program.
- [x] **Conformance:** `modules/cross_module/` (`main.lang` `use`s `App.Models.User` from `models.lang`; the real class's constructor *and* a `greeting()` method run — an opaque stub has neither). `modules/namespace_and_use.lang` still passes on the opaque-stub fallback (single file, no sibling modules). Loader unit tests (resolve, opaque fallback, entry parse-error sourcing). Suite **67 passed**; differential **63 matched / 0 skipped / 100% / zero divergence**.

## M1.9.2 — in progress (visibility done)

- [x] **`pub` visibility (module-private by default).** New `pub` keyword (lexer `PubKw`); `is_public` on class/record/enum/fn decls (AST + parser — `pub` is parsed after any decorators, before the keyword; `pretty` renders `pub ` only when set, so non-`pub` snapshots are byte-identical). The linker now resolves *strictly* once a namespace is found: only a `pub` declaration is importable. Importing a module-private declaration or one the module does not declare is the new **E0019 `UnresolvedImport`** (a private import and a typo'd one give distinct messages, both E0019, on the imported name's span). A `use` whose namespace **no** module provides still falls back to the opaque stub (so single-file `namespace_and_use.lang` is unaffected). Conformance `modules/private_import/` (E0019 at 7:16). Loader unit tests (`importing_a_private_declaration_is_e0019`, `importing_a_missing_export_is_e0019`); parser `parses_pub_visibility`. The `modules/cross_module` `User` is now `pub`. Suite **68 passed**; differential unchanged (a load error is excluded from the differential like a parse failure). No `unsafe` touched.
- [x] **Unknown-type `E0013`.** The checker's collect pass now records every legal annotation referent — declared records/classes/enums, names brought in by a `use` (whether the linker merged the real declaration or left an opaque stub), and (per declaration) its generic `<T, ...>` parameters — alongside the lattice built-ins and the bare prelude spellings (`list`/`map`/`set`/`Ordering`). Every annotation (parameter, return, field, enum backing, generic argument) is walked; a name resolving to none of those is **E0013** on the offending name, recursing into generic arguments so `List<Ghost>` flags `Ghost`. This lit up one genuinely type-dishonest corpus annotation — `coalesce_default.lang`'s `fn find(): ?User` actually returns `?string` — now corrected. Conformance `types/unknown_type.lang` (E0013 at 7:18). Checker unit tests (`undeclared_type_annotation_is_e0013`, `imported_type_annotation_is_not_flagged`, `generic_parameter_is_a_legal_type`). Suite **69 passed**; differential **64 matched / 0 skipped / 100% / zero divergence**. No `unsafe` touched.
- [x] **Name-collision / ambiguity `E0020`.** The linker now rejects an imported name that collides with another top-level name in the entry — a second import of the same name, or one shadowing a local declaration — at the offending `use` name's span (entry source, so it renders correctly with no `SourceMap`). A pre-scan of the entry's own declaration names makes the check order-independent. New code **E0020 `NameCollision`** (append-only). Conformance `modules/name_collision/` (E0020 at 8:16); loader unit tests (`an_import_colliding_with_a_local_declaration_is_e0020`, `two_imports_of_the_same_name_collide`). Suite **70 passed**; differential **64 matched / 0 skipped / 100% / zero divergence**.
- [x] **Cross-module type checking** falls out of the merge: the harness runs `lang_check::check` over the *linked* program, so a type error in a merged-in declaration (its annotations, bodies, exhaustiveness) is caught exactly as if it were local — no module-aware checker needed.
- [ ] **`SourceMap` (deferred — latent).** A *check/runtime* diagnostic that lands on a merged-in declaration currently renders against the entry source (wrong file/line), because [`Span`] carries byte offsets but no `SourceId` and the merged declarations keep their module-local offsets. This is latent: a *positive* linked program produces no such diagnostic, and every negative case so far (E0019/E0020 import errors) is raised in the linker against the entry source, where it renders correctly. A fully-correct fix needs global-coordinate spans — either a `SourceId` on every `Span` (deliberately avoided: it would touch every span the lexer/parser builds) or a post-parse AST fold rebasing each merged subtree's spans into a combined `SourceMap` (the parser slices source text by local span, so tokens cannot simply be emitted pre-shifted). Tracked here; deferred until a non-latent need (a real cross-module *body* error to surface) justifies the span-rebasing work.

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
