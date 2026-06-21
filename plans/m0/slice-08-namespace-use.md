# Slice 8 — `namespace` / `use`

Status: done

## Goal
Parse `namespace` and `use` declarations and resolve imported names well enough for the §14 demo to run.

## Scope
- In: `namespace App.Orders;`; `use App.Models.User;`; grouped `use App.Billing.{Invoice, Receipt};`; a trivial name table mapping imported names to stubs/builtins so references resolve.
- Out: real module loading, file-based module resolution, visibility across modules (all M1).

## Checklist (vertical slice)
- [x] Grammar / AST: namespace decl, use decl (single + grouped), dotted paths.
- [x] Checker rule: n/a.
- [x] Bytecode: n/a.
- [x] Eval op: build a trivial name table; resolve a `use`d name (e.g. `User`) to a stub type so the demo evaluates; keep AST nodes present for M1.
- [x] Conformance cases: a program with `namespace` + `use` that runs.
- [x] Snapshots: AST for a namespaced program.

## Notes / traps
- Do not skip parsing these — the §14 demo opens with them and won't parse otherwise.
- Resolution is a stub in M0; real resolution is an M1 task.

## Definition of done
- Conformance case with `namespace`/`use` runs green.
- fmt/clippy clean; zero `unsafe`.

## Outcome (done)
New keyword tokens `namespace`/`use`; new statements `Stmt::Namespace { path }` and
`Stmt::Use { path, names }` (with `UseName { name, span }`). The `use` parser handles both
the single form (`use App.Models.User;` → path `App.Models`, name `User` — the last segment
is the leaf) and the grouped form (`use App.Billing.{Invoice, Receipt};` → path `App.Billing`,
names `Invoice`/`Receipt`). Dotted paths are parsed via a `.`-led tail that is either another
id or the trailing `{ group }` (matched first in the `choice`, so a clean `.id` never
half-consumes a `.`); `build_use` splits the result into prefix + leaves.

**Eval (stub resolution):** `namespace` is a no-op (no module scoping in M0). `use` registers
each imported name as an **opaque stub `TypeDef`** (`opaque: true`, empty field set). An
opaque type's all-fields literal accepts *any* named fields and skips the unknown-field and
full-init checks (its real shape is unknown until M1 module loading); `..` spread copies the
whole base; `ObjectValue::display` shows the actual bag (key order) rather than declared
fields. This is what lets the §14 demo construct and read an imported `User`
(`User { name: "Ada" }`, `customer.name`) even though only a stub exists.

Verified the §14 demo's namespace/use/`User` opening now runs end-to-end (constructed `User`,
field access through the stub, class fields typed `User`/`List<Item>`). 3 eval tests, 1 lexer
test, 1 AST snapshot, 1 conformance case (21 total). fmt/clippy clean; zero `unsafe`.
