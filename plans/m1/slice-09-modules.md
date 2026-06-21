# Slice M1.9 — Modules / namespaces / `use` resolution

Status: todo

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
