# Slice 8 — `namespace` / `use`

Status: todo

## Goal
Parse `namespace` and `use` declarations and resolve imported names well enough for the §14 demo to run.

## Scope
- In: `namespace App.Orders;`; `use App.Models.User;`; grouped `use App.Billing.{Invoice, Receipt};`; a trivial name table mapping imported names to stubs/builtins so references resolve.
- Out: real module loading, file-based module resolution, visibility across modules (all M1).

## Checklist (vertical slice)
- [ ] Grammar / AST: namespace decl, use decl (single + grouped), dotted paths.
- [ ] Checker rule: n/a.
- [ ] Bytecode: n/a.
- [ ] Eval op: build a trivial name table; resolve a `use`d name (e.g. `User`) to a stub type so the demo evaluates; keep AST nodes present for M1.
- [ ] Conformance cases: a program with `namespace` + `use` that runs.
- [ ] Snapshots: AST for a namespaced program.

## Notes / traps
- Do not skip parsing these — the §14 demo opens with them and won't parse otherwise.
- Resolution is a stub in M0; real resolution is an M1 task.

## Definition of done
- Conformance case with `namespace`/`use` runs green.
- fmt/clippy clean; zero `unsafe`.
