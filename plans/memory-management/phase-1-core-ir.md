# Phase 1 — Core IR + lowering + IR interpreter

Build the foundation everything else rests on: a shared, lowered **Core IR** in A-normal form, a
faithful `AST → Core IR` lowering, and a Core-IR **tree-interpreter** validated against the existing
AST-walker. No reference-counting change yet — this phase only proves the IR is a *faithful* second
representation of the language, so later phases can transform it with confidence.

## 1.1 `lang-ir` — the Core IR (ANF)

New crate `lang-ir` (depends on `lang-ast`, `lang-span`, `lang-types`, `lang-diagnostics`; no backend
dep). A-normal form: **every intermediate value is explicitly named**, every operand is an atom (a
named value or literal). Structured control flow preserved (no arbitrary goto).

- **Values & lets:** `let v = <op>; …` where `<op>` is a primitive (literal, binop, call, field load,
  constructor, narrow, etc.) over *atoms*. Nesting like `acc.x + 1` becomes `let t0 = acc.x; let t1 =
  t0 + 1`. **This is the whole point**: `t0` (the receiver-field temporary that had no AST node and
  blocked P-REUSE) is now a named IR value with a definition and a computable last use.
- **Control flow:** structured nodes for `if`/`match`/`while`/`for`/`break`/`continue`/`return`/`?`,
  plus `let`-regions delimiting scopes. Closures lower to an explicit closure-construction op capturing
  named values. Keep it structured (not a raw CFG) so backward last-use (Phase 3) stays a structured
  walk and the tree-interpreter stays simple.
- **Reserved RC slots:** the IR node types carry optional `dup`/`drop`/`reuse-token`/`in-place`
  annotations, *empty in this phase*. Phase 3 fills them; defining them now fixes the shape both
  backends will consume.
- **Spans throughout:** every IR node retains its source span (diagnostics + the destructor spec's
  "last use point" + cross-referencing to type facts).

## 1.2 Lowering — `AST + Checked → Core IR`

A pass (in `lang-ir` or a `lang-lower` crate) consuming the AST and the type-checker output (so it
knows concrete types, which fields are heap-bearing, generic instantiations — needed later for RC
relevance). It is a **pure, total** function: every program in the VM subset lowers; nodes outside the
subset lower to an explicit `Unsupported`-equivalent so the differential's skip behavior is preserved.

- Faithful, semantics-preserving: evaluation order (the tree-walker's order, which the VM already
  matches) is made explicit by the `let`-sequencing. This *fixes* evaluation order as IR structure
  rather than re-deriving it per backend — a correctness win on its own.

## 1.3 Core-IR tree-interpreter (a new eval path)

Add an IR interpreter to `lang-eval` (or a sibling) that walks the Core IR with the **same Rust-`Rc`
value model** the AST-walker uses. Reclamation stays exactly as today (Rust `Rc` drop + globals-only
destructors) — no RC change.

## 1.4 Faithfulness proof (the gate that makes the IR trustworthy)

- **Differential the new IR-interpreter against the existing AST-walker** across the *entire*
  conformance corpus: identical stdout/exit for every program. This is a transitional, internal
  differential (old-eval vs new-IR-eval) on top of the existing eval-vs-VM differential.
- **IR golden tests:** snapshot the lowered Core IR for a representative corpus (stable textual dump),
  so lowering changes are reviewable and regressions visible.
- Keep the AST-walker in place — it is the reference oracle the IR path is validated against, retained
  at least through Phase 4 (README §2 tradeoff).

## Verification gate

- `lang-ir` + lowering + IR-interpreter build; IR golden dumps reviewed.
- **old-eval ≡ new-IR-eval** on the full corpus (the faithfulness differential); eval-vs-VM
  differential unchanged; leak oracle unchanged.
- The VM is untouched this phase (still compiles AST → bytecode) — so the existing differential and all
  behavior are by definition unaffected on the VM side.
- clippy + fmt clean. (No new unsafe; miri N/A this phase.)
