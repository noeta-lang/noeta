# P2.7 — semantic roles (`@role(...)` + `roles_of()`)

Status: **DONE** (conformance 201 / differential 195 / 0-skipped / backends agree / miri-clean).
Branch `types-inferred-static`. The last slice of `plans/attributes/pass-2-reflection.md` (§9.13's
labeled dependency graph). New diagnostic code **E0031**; next free **E0032**.

An attribute may confer a typed architectural **role** on every declaration it annotates. The
compiler indexes `(declaration, Role)` at manifest-build time and `roles_of()` surfaces it, so the
dependency graph is queryable in architectural terms ("every entry point," "does this trust
boundary reach a sink").

## Design (decided with the user)

The plan as written said *"the compiler evaluates `role()` at manifest-build time"* — but that is
the **comptime path the user rejected in P2.5** (the `valid_target` predicate): the checker has no
evaluator, and `lang-eval`→`lang-check` (from P2.3) makes the reverse a cycle. So the role is
**declarative**, mirroring P2.5's `@attribute(...)` exactly:

```
@attribute(Function, Method)
@role(EntryPoint)                              // Route confers the EntryPoint role
type Route = { path: string };

for b in roles_of() {                          // List<RoleBinding> — { target: string, role: Role }
    match b.role { Role.EntryPoint => ..., Role.Sink => ..., _ => ... }
}
```

- **`@role(Role)`** is a `@derive`-family directive (`@name(ident-list)` grammar, arg-list, **zero
  new tokens**) carrying exactly one blessed `Role` variant.
- **`Role`** is a prelude enum (the `Type`/`Ordering` template): `EntryPoint`,
  `PersistenceBoundary`, `TrustBoundary`, `Sink`, `Layer` — all **payload-free** (a parameterized
  `Layer(name)`/`Custom(name)` would evaluate per use site ⇒ comptime ⇒ deferred).
- **`roles_of()`** is a keyword builtin (symmetric with `attributes_of`/`type_of`), no type-arg,
  returning `List<RoleBinding>` where `RoleBinding { target: string, role: Role }` is a prelude
  record.
- A role rides on an attribute, so `@role` requires the record also be `@attribute`; `@role` on a
  non-attribute record, on a class/enum, with an unknown variant, or with ≠1 variant → **E0031**.

The role only matters for attributes (records), so — like `@attribute` — placing `@role` on a
class/enum is records-only-rejected, not honored. A parameterized/computed role and the original
`impl SemanticRole { fn role() }` spelling are deferred (gated on comptime), recorded alongside the
deferred `valid_target` predicate.

## Mechanism (one new op + one new prelude enum + one new prelude record)

The `(declaration, Role)` index is built into the **shared `reflect::ReflectionInfo`** (so both
backends agree by construction, like the attribute manifest): a new `roles: Vec<RoleRecord>` field,
`RoleRecord { target, role }`. `reflect::build` harvests each attribute record's `@role` variant
(`role_of`) and joins it with the manifest — every *use* of a role-tagged attribute is indexed
(deduped). `roles_of()` materializes it identically in both backends:

- **One new op** `Op::RolesOf { dst }` → `materialize_roles()`: builds `RoleBinding` records (fresh
  `TypeDef`/`Shape`) with a `string` target and a `Role` enum value (`make_role` / `builtin_enum`,
  the payload-free `make_ordering` template). The freshly-built list/record/enum refcounts transfer
  into the list cleanly (miri-clean, identical structure to `materialize_attributes`).
- **`Role` + `RoleBinding`** register in the checker `register_role_prelude` (the `Attributed`/
  `Type` template) so `roles_of()` types as `List<RoleBinding>`, `b.role` is `Role`, and `match`
  arms over `Role.*` type-check; "Role" joins `PRELUDE_TYPES`.

Single source of truth for the role vocabulary: `lang_ast::reflect::{ROLE_ENUM, ROLE_BINDING,
ROLE_VARIANTS}`, shared by the checker validation, the prelude registration, and both backends'
materialization — so the `@role` tag, the enum users match on, and the materialized value all agree.

## Touch list

- **Diagnostics** `lang-diagnostics`: `InvalidRole` → **E0031** (`ALL` + `code()`).
- **AST** `lang-ast`: `Expr::RolesOf { span }` (+ `span()` + pretty `(roles_of <span>)`);
  `RecordDecl.role`/`ClassDecl.role: Option<Vec<(String, Span)>>`.
- **reflect** `lang-ast::reflect`: `roles: Vec<RoleRecord>` on `ReflectionInfo`; `RoleRecord`;
  `ROLE_ENUM`/`ROLE_BINDING`/`ROLE_VARIANTS`; build the index in `build`.
- **Lexer** `lang-lexer`: `#[token("roles_of")] RolesOfKw` (+ name/describe).
- **Parser** `lang-parser`: `roles_of` combinator (keyword + `()`); partition `"role" => role`;
  thread `role` through `attach_decorators`; `role: None` at the two decl construction sites.
- **Checker** `lang-check`: `register_role_prelude`; `"Role"`/`"RoleBinding"` in `PRELUDE_TYPES`;
  `record_role` (E0031) called in `collect`; `@role` on a class → E0031; `Expr::RolesOf` synth →
  `List<RoleBinding>`.
- **Bytecode** `lang-bytecode`: `Op::RolesOf { dst }` + disasm.
- **Compiler** `lang-compiler`: lower `Expr::RolesOf` → `Op::RolesOf`; `freevars` (3 no-op arms).
- **VM** `lang-vm`: `materialize_roles` + `make_role`; `Op::RolesOf` dispatch.
- **Eval** `lang-eval`: `materialize_roles`; `Expr::RolesOf` arm.
- **Conformance** `tests/conformance/reflection/`: `roles_of` (two roles, `match` on `b.role`,
  differential), `role_unknown`/`role_requires_attribute`/`role_on_class` (E0031). Parser snapshot
  `roles_of_parses`. Checker units (synth clean + 3 E0031 cases). VM unit `roles_of_materializes`.
- **Docs**: `docs/resources/02-syntax.md` §9.7 (rewrote the evaluated-`role()` block to declarative
  `@role`), `01-architecture.md` (role bullet); memory (`attribute-system`, pass-2 plan).

## Verification (before commit)

- `cargo run -q -p lang-cli -- test` → 201 passed / 0 failed.
- `cargo run -q -p lang-cli -- test --differential` → 195 matched / **0 skipped** / backends agree.
- `cargo test --workspace` · `cargo clippy --all-targets` · `cargo fmt --all --check` → clean.
- `cargo +nightly miri test -p lang-vm roles_of_materializes` → clean.
