# Convert stringly-typed trait dispatch to the `BuiltinTrait` enum

Status: done (`0a57be1` → `fb516f9`)

**Done in three commits.** `BuiltinTrait` was a *struct*, not an enum, so step 1
was to convert it: (1) `0a57be1` made it a **fieldless enum** with per-variant
metadata behind one authoritative `info()` match (`lookup` → `from_name`, field
access → method calls, `operator_trait` returns it by value); (2) `ccf7e75`
threaded it through the satisfaction path — `trait_impls: HashMap<String,
HashSet<BuiltinTrait>>` (mapped at the `record_trait_impls` boundary),
`satisfies`/`builtin_satisfies`/`operand_satisfies_operator`/`unbounded_type_param`/
`report_operator_error`/`required_operator_trait` take/return `BuiltinTrait`, and
`builtin_satisfies` matches the enum **exhaustively** (no `_` arm); (3) `fb516f9`
added `Type::is_arith_numeric()` (IntN-inclusive, distinct from `is_numeric`) to
dedup the numeric sets and closed the private-field `verb: &str` into a
`FieldAccess` enum. Left the single `json`/`parse` special-case (one guarded
check, not a dispatch table). 189 checker tests + differential 417/0 unchanged,
clippy/fmt clean.

`lang-types` already exports a `BuiltinTrait` enum (with `BUILTIN_TRAITS`,
`operator_trait`, …), yet the checker dispatches on trait **name strings**
throughout: `trait_impls: HashMap<String, HashSet<String>>`,
`check_trait_impl(trait_name: &str)`, `operand_satisfies_operator(_, trait_name:
&str)`, `builtin_satisfies(_, trait_name: &str)` matching `"Comparable"`/`"Add"`,
etc. This track makes illegal trait names unrepresentable by threading the enum
where strings flow today. Also cleans up the adjacent stringly-typed module
dispatch (`module == "json" && func == "parse"`) and the private-field `verb:
&str` (`"read"`/`"assign"`/`"set"`) if cheap.

## Goal

Built-in trait identity is a `BuiltinTrait` value, not a `String`, from the point
a `@derive`/`impl`/operator resolves it through satisfaction checking. A typo like
`"Comparble"` becomes a compile error, not a silently-unsatisfied bound. Behavior
byte-identical.

## Scope

- **In:**
  - `trait_impls: HashMap<String, HashSet<BuiltinTrait>>` (user type name → the
    built-in traits it derives/impls). Update `record_trait_impls` and its ~4
    call sites to map `&str` → `BuiltinTrait` once, at the boundary.
  - `check_trait_impl`, `operand_satisfies_operator`, `unbounded_type_param`,
    `satisfies`, `builtin_satisfies`, `required_operator_trait` — take/return
    `BuiltinTrait` instead of `&str`; the string→enum parse happens once where a
    trait name enters from the AST (`DeriveSpec.name`, `impl Trait`, an operator's
    `operator_trait(op)`).
  - `builtin_satisfies`: the numeric-set duplication across the arith arms
    (`Int|Float|F32|IntN{..}`) can now key on the enum; factor a
    `Type::is_arith_numeric()` helper **carefully** — note that `Type::is_numeric()`
    excludes `IntN`, so do not reuse it (that was a deliberate skip in the cleanup
    arc — see the branch history).
- **Optional adjacent cleanups (same stringly-typed smell):**
  - Module dispatch `module == "json" && func == "parse"` → a small enum or a
    typed lookup.
  - `report_private_field(verb: &str)` with `"read"`/`"assign"`/`"set"` → a 3-variant
    enum.
- **Out:** any change to which traits exist or what they mean; the on-disk
  attribute manifest format.

## Design

`BuiltinTrait` and `operator_trait` already exist, so the parse boundary is thin:
one `BuiltinTrait::from_name(&str) -> Option<BuiltinTrait>` (add if absent) called
where a trait name first enters. Unknown names still produce the existing E0014
"unknown trait" diagnostic — the enum conversion returns `None` there, so error
paths are unchanged. Interior code then matches the enum exhaustively, so adding a
built-in trait forces every dispatch site to be updated (the compiler enforces
coverage — the whole point).

## Risks & constraints

- Touches the checker's trait-satisfaction core, which feeds E0014/E0015/E0025/
  E0027 and all operator-trait dispatch. Medium risk: the 189 checker tests (which
  pin every static-error class) plus the differential are the net.
- Do the `trait_impls` map type change and its call sites in one commit, then the
  `satisfies`/`builtin_satisfies` signatures in another, so each stays green.
- Best done **independently of the checker file split** (`split-checker-lib.md`) —
  same file, different code; do whichever first, then the other.

## Checklist

- [x] `BuiltinTrait::from_name` + one parse boundary (plus `BuiltinTrait` is now a fieldless enum)
- [x] `trait_impls` keyed by `BuiltinTrait`; `record_trait_impls` maps at the edge
- [x] `satisfies`/`builtin_satisfies`/`operand_satisfies_operator`/`unbounded_type_param`/
      `report_operator_error`/`required_operator_trait` take/return `BuiltinTrait`
- [x] `Type::is_arith_numeric()` helper (IntN-inclusive) removes the numeric-set dup
- [x] (optional) private-field-`verb` → `FieldAccess` enum (module-dispatch left as-is: one guarded check)
- [x] 189 checker tests green; differential 417/0, backends agree; clippy/fmt clean

## Definition of done

No built-in trait identity flows as a `String` through the checker's satisfaction
path; unknown trait names are rejected only at the single parse boundary (E0014
unchanged); the 189 checker tests and the differential are unchanged and all gates
green.
