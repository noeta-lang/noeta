# Conditional narrowing & expressions (`is` · `if…then…else` · `??=`)

Status: **complete**. Conformance 176 / differential 170 matched / 0 skipped / backends agree. Branch `types-inferred-static`.

This small track closes the two items the inferred-static type-system track (`plans/types/`) deferred — **exhaustive `match` over a union** (from S8) and the **ternary / conditional expression** (deferred after the S-arc) — plus one user-requested companion, the **`??=` coalescing assignment**. A design discussion settled the surface before implementation; the through-line is **`is` for type tests, `then`/`else` for conditional expressions, and the loaded `?`/`:` sigils left untouched**.

## The shared primitive

Everything rides one new runtime op, **`Op::IsType`** (a bool head-constructor test), which reuses the S6 narrowing matcher end to end (`NarrowTarget`, `narrow_matches`/`runtime_matches`/`narrow_target`). It lands in both backends, so `--differential` stays at **0 skipped** by construction. The two conditional-expression forms (`if…then…else`, `??=`) are *pure parser desugar* and add no runtime surface.

No new diagnostic codes: exhaustiveness reuses **E0011**, an unknown tested type reuses **E0013**. Next free stays **E0029**.

## Slices (each a green commit)

| Slice | What | Mechanism |
|---|---|---|
| **N1** | `is` type-test operator — `x is T` → `bool` | New keyword `is`; `Expr::TypeTest`; parser postfix at the comparison tier (bp 5); checker synth → `bool` (no source gate — a test is benign even on a concrete value, unlike `.as<T>()`'s E0028); `Op::IsType` in both backends. |
| **N2** | `is`-patterns in `match` + exhaustiveness + arm narrowing | `Pattern::IsType`; eval/compiler lower via the shared matcher (compiler reuses `Op::IsType` + `Op::JumpIfFalse`, no new match op); `synth_match` narrows an identifier scrutinee in an `is T` arm; `check_exhaustive` makes a union a **closed** domain (exhaustive without `_`) and `dyn` an **open** one (needs `_`). |
| **N3** | `if cond then a else b` conditional expression | New keyword `then` (forks statement `if … { … }` from expression `if … then …` at parse time). Pure parser desugar to a two-arm `match`: a `cond is T` form lowers to a type-pattern match (so the `then` arm narrows); any other condition to a `true`/`false` match. |
| **N4** | statement-`if` flow narrowing | Checker-only: `if ident is T { … }` narrows `ident` to `T` in the then-body, gated on the body not reassigning it (reuses `reassigns`). Observable on a union operand (`x + 1` on `int \| string` is E0007 bare, clean when narrowed). |
| **N5** | `??=` coalescing assignment | New `??=` token; the compound-assignment desugar's op carrier widens to `AssignKind` (Plain / Binary / Coalesce). `x ??= y` ⟶ `x = x ?? y`, reusing `Expr::Coalesce` (so it short-circuits). |

## The resulting vocabulary

| Want | Form |
|---|---|
| is the value a `T`? | `x is T` (bool) |
| value, or default if `none` | `x ?? y` |
| fill a var with a default if `none` | `x ??= y` |
| value from an arbitrary condition | `if c then a else b` |
| value, narrowing a `dyn`/union | `if x is T then a else b` |
| narrow in a block | `if x is T { … }` |
| exhaustive discrimination | `match x { is T => …, … }` |

`as` stays the *narrowing-to-`?T`* keyword (`x.as<T>()`); `is` is the *test*. The closed-vs-open distinction a union has over `dyn` is now operational: a union `match`/`if` is exhaustive without a `_`, a `dyn` one requires it.

## Deliberately out of scope (recorded)
- **Negative narrowing** in the `else`/`_` arm (`x: A | B`, else ⇒ `x: B`).
- **Narrowing through `&&`** — only a bare `ident is T` condition narrows.
- **Tightening `match`'s arm-type join** to a union or a mismatch error (today it takes the first non-gradual arm) — a separate `match` change; `if…then…else` inherits it.
- **A binder for a non-identifier scrutinee** (C#-style `is T n`) — flow-narrowing covers the identifier case; bind a computed scrutinee to a name first.

## Next
Per the standing directive: the **attribute-system pass**, then the **perf-related deferred items**.
