# Semantic roles, generalized: `@semantic` enums + `@role(Enum.Variant)`

Status: **planned** (not started). Branch `types-inferred-static`. **Supersedes the shipped P2.7**
(commit `c6a98c2`, the closed 5-variant `Role` enum) with the user-designed extensible model.
Depends on [abstract-kind-types](../types/abstract-kind-types.md) (`role: Enum`).

## What changes from the shipped P2.7

P2.7 shipped a closed, built-in `Role` enum (5 blessed variants) and `@role(Variant)`. That can't
express framework/app-specific roles. The redesign makes the role vocabulary **user-extensible**
without raw strings or a `Custom` wrapper: any enum a user marks `@semantic` becomes role-eligible,
and `@role(...)` references one of its variants. This is the Java/C# annotation-takes-an-enum pattern
(`@Retention(RetentionPolicy.RUNTIME)`, `[AttributeUsage(AttributeTargets.Method)]`).

```
@semantic                                  // promote a user enum to role-eligible
enum WebRole { Controller, Middleware, ErrorHandler }

@attribute(Function, Method)
@role(WebRole.Controller)                   // a custom role
type Route = { path: string };

@attribute
@role(Semantic.Sink)                        // a built-in role
type Persist = { table: string };

for b in roles_of() {                        // role: Enum  (the abstract supertype)
    match b.role {
        Semantic.EntryPoint => registerEntry(b.target),
        WebRole.Controller  => registerController(b.target),
        _                   => skip(),       // Enum is open ⇒ needs `_`
    }
}
```

## Decisions (locked with the user)

- **Roles are enum-only.** Structured data belongs to attributes, not roles (a role stays a small,
  matchable nominal label). Records/maps as role values are explicitly **not** in scope.
- **`@semantic`** marks an *enum* as role-eligible (records/classes → error). The built-in
  vocabulary becomes a `@semantic enum Semantic { EntryPoint, PersistenceBoundary, TrustBoundary,
  Sink, Layer }` (implicitly semantic), renamed from `Role`. `@role(Semantic.EntryPoint)`.
- **`@role(Enum.Variant)`** references a **fieldless** variant of a `@semantic` enum. A payload
  variant is rejected — its payload would have to be built per use site (genuine comptime); this is
  a real boundary, flagged here rather than buried, and the only thing roles defer.
- **`role: Enum`** (not `dyn`): `roles_of()` returns `List<RoleBinding>`, `RoleBinding { target:
  string, role: Enum }`. Since `Enum <: dyn`, this is non-breaking if we ever loosen it.
- **Multiple `@role(...)` per declaration** allowed — a thing can be both an `EntryPoint` and a
  `TrustBoundary`; each becomes its own `(declaration, role)` binding.

## Validation (E0031, reuse the existing code)

`@semantic` on a non-enum → E0031. `@role(X.Y)` where `X` is not a `@semantic` enum, `Y` is not a
variant of `X`, or `Y` carries fields → E0031. (The built-in `Semantic` enum is implicitly
`@semantic`.)

## Mechanism (revises the P2.7 implementation)

- **`@semantic` directive** (`lang-parser` + `lang-ast`): a `@derive`-family directive on an enum
  (zero new tokens, like `@attribute`); `EnumDecl` carries a `semantic: bool` (or a span for error
  reporting); checker registers `self.semantic_enums: HashSet<String>` in `collect`. `Semantic`
  registers as a built-in `@semantic` enum in `register_role_prelude` (rename of the P2.7 helper).
- **`@role(Enum.Variant)` parsing**: today `@role` reuses the identifier-list grammar (P2.7). It now
  takes a **qualified path** `Ident.Ident`; carried on `RecordDecl.role` as `Vec<((String,String),
  Span)>` (enum, variant) — or keep the directive shape and parse the dotted pair. The checker's
  `record_role` validates against `self.semantic_enums` + fieldless-variant.
- **Reflection** (`lang-ast::reflect`): `RoleRecord { target, enum_name, variant }` (was `target,
  role`); `reflect::build` harvests `(enum, variant)` from each attribute's `@role` and joins with
  the manifest (dedup). `roles_of()` materializes `RoleBinding { target, role }` where `role` is the
  **actual enum value** `enum_name.variant` (`builtin_enum`/`make` shape, payload-free) — both
  backends build it identically (the P2.7 `make_role` generalizes to any enum name).
- **`RoleBinding.role` type** is `Enum` (from the abstract-kind-types plan); the checker registers
  `RoleBinding { target: string, role: Enum }`.
- **Migrate** the P2.7 conformance fixtures + checker/VM units to the new surface (`@role(Bogus)` →
  `@role(Semantic.Bogus)`/unknown-enum cases; add a `@semantic` user-enum fixture; a payload-variant
  → E0031; multiple roles on one declaration).

## Touch list

`lang-ast` (`EnumDecl.semantic`, `RecordDecl.role` shape, `reflect::RoleRecord`), `lang-parser`
(`@semantic`, qualified `@role`), `lang-check` (`semantic_enums`, `record_role` rework, `Semantic`
prelude, `RoleBinding.role: Enum`), `lang-eval` + `lang-vm` (`materialize_roles` builds the named
enum value), docs (`02-syntax.md` §9.7 — already declarative, update to the `@semantic` model;
`01-architecture.md` role bullet), memory.

## Verification & sequencing

Conformance + differential (0-skipped, agree), workspace/clippy/fmt/miri. **After**
abstract-kind-types. Standard trailers.
