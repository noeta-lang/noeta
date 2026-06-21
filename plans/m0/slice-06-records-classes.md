# Slice 6 — Records & classes

Status: done

## Goal
Structural records and classes with fields, methods, `.` access, the all-fields literal, named constructors, and `..` structural update.

## Scope
- In: `type Item = { price: float, qty: int }` structural records; `class` with fields declared in body (immutable default, `mut` field opt-in), methods, `.` field/method access; the all-fields literal `Order { id: ..., ... }` (must set every field); named constructors as ordinary associated fns returning `Self` (`new`, `draft`); `..spread` structural update `Money { amount: 300, ..a }`.
- Out: trait dispatch / operator overloading / derives (M1); packed value types (M2); literal visibility enforcement (parse the intent; full private-by-default checking is M1).

## Checklist (vertical slice)
- [x] Grammar / AST: record type alias, class decl (fields + methods), all-fields literal, spread in literal, associated-fn calls (`Type.fn(...)`), member access `.`.
- [x] Checker rule: n/a — the all-fields literal **requires every field at eval time** (the full-initialization choke point) and errors (`E0009`) if a field is missing.
- [x] Bytecode: n/a.
- [x] Eval op: object values (field bag); method dispatch via `.`; static-vs-instance disambiguation (left side is a type vs a value); all-fields literal construction; `..` spread fills unnamed fields shallowly.
- [x] Conformance cases: construct via `new`, call a method, access a field, structural update, and a **negative** case for a missing field in the literal.
- [x] Snapshots: AST for a class program.

## Notes / traps
- Consider splitting 6a (records) and 6b (class methods) into separate commits if the diff is large.
- Constructors are inferred (no `self`, returns enclosing type); transformations take `self` — keep this distinction in the AST for M1 tooling even if not enforced now.

## Definition of done
- Conformance cases pass for record/class construction, methods, `.` access, structural update, and missing-field error.
- fmt/clippy clean; zero `unsafe`.

## Outcome (done)
Records (`type X = { ... }`) and classes (`class X { fields... methods... }`) landed
together (shared object machinery; a record is a class with no methods). New tokens
`type`/`class`/`..`; new AST nodes `RecordDecl`/`ClassDecl`/`FieldDecl` and
`Expr::Object`(`ObjectLit`/`FieldInit`). The all-fields literal `Type { f: v, ..base }`
constructs a `Value::Object`; the evaluator requires every declared field to be set
(`E0009 MissingField`) and rejects unknown fields (`E0005`). Dispatch: `Type.fn(args)`
is a static/associated call (no instance); `value.method(args)` binds the instance's
fields directly into the method scope (plus `self`) so methods read `items`/`status`
without a prefix. `..` spread fills unnamed fields shallowly; objects compare
structurally. Constructors-are-just-functions: `new`/`draft` are ordinary methods
returning the type, distinguished from instance methods only at the call site.

**Parser note (no-struct-literal ambiguity):** an object literal `Ident { ... }` collides
with `if x { }` / `for x in xs { }` / `match x { }` block braces. Resolved the M0 way by
requiring the object body to be **non-empty** — an empty `{}` is never an object literal,
and a `{` whose contents are statements (not `name: value`) fails the field parse and
falls back to a bare ident. Trade-off: a zero-field all-fields literal (`Type {}`) is not
expressible in M0 (degenerate; use a constructor). 11 eval tests; 1 AST snapshot; 1 lexer
test; 4 conformance cases (16 total).
