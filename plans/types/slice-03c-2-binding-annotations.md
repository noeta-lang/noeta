# Slice S3c.2 — Optional binding type annotations

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`). **Part of:** S3c (inference completion), second of four sub-slices. **Depends on:** S3c.1. **Determinism posture:** the annotation is checked statically and **erased at runtime**, so both backends run an annotated binding identically; the new end-to-end corpus case lands at **114 / 0 skipped**.

## Goal

Give bindings an optional type annotation (`name: Type = value`, `mut name: Type = value`). This matches the locked philosophy — "signatures required at boundaries, optional in bodies" — and is the prerequisite that makes the later hard `E0023` (S3c.4) *fixable*: a value inference cannot resolve can be pinned by writing its type, exactly as Rust's `let v: Vec<i32> = Vec::new();`. Until this slice, `Stmt::Binding` had no annotation field at all, so an un-inferable binding had no escape hatch.

## What shipped

### AST + parser
- `Stmt::Binding` grows `ty: Option<TypeRef>` — absent for the common `x = …` form.
- The `mut` binding parser accepts `mut name: Type = value`.
- The `assign_or_expr` statement parser threads an optional `: Type` between the left-hand name and `=`, producing an annotated `Binding` only for a fresh `name: Type = value`. A stray `name: Type;` with no value is a parse error ("a type annotation requires a value"). No ambiguity: a `:` at statement scope can only follow a binding name (map-literal `:` is consumed inside the expression parser).

### Checker
The `Stmt::Binding` arm checks an annotated value against the annotation (real `check`-mode expectation) and binds the binding at the annotated type; the annotation is validated by the existing `check_type_ref` (`E0013` on an unknown type). Un-annotated bindings stay inference-only (open expectation), exactly as before.

## Traps handled
- **Annotation + value double diagnostics**: `x: Ghost = 5` correctly reports both `E0013` (unknown type) and `E0007` (int not assignable to the unknown type) — each is a genuine, separate problem.
- **Pretty-printer**: the S-expression dumper already renders *no* type annotations anywhere (even parameters print names only), so a binding ignoring `ty` via `..` is consistent — no change, and existing pretty output is byte-identical.
- **Runtime erasure**: every backend consumer destructures `Stmt::Binding` with `..`, so the annotation is transparently erased at runtime; the end-to-end corpus case proves both backends agree.

## Files
- `crates/lang-ast/src/lib.rs` — `Stmt::Binding.ty`.
- `crates/lang-parser/src/lib.rs` — optional annotation in the `mut` and `assign_or_expr` binding parsers.
- `crates/lang-check/src/lib.rs` — annotated-binding checking + binding at the annotated type.
- `crates/lang-check/src/tests.rs` — 3 new tests (value-against-annotation, unknown annotation type, type-flows-to-later-uses).
- `tests/conformance/bindings/annotated_binding.lang` (run, both backends) + `annotated_binding_mismatch.lang` (`E0007`).

## Determinism / oracle posture
Conformance **120 passed**; differential **114 matched / 0 skipped / backends agree**. 53 checker tests (3 new), 17 parser tests; clippy + fmt clean.

## Definition of done — met
Bindings accept an optional, checked type annotation; the value is checked against it and the binding bound at it; the annotation is erased at runtime (both backends agree); a mismatch is `E0007`, an unknown annotation type `E0013`. The backward-inference solver (S3c.3) and the `E0023` endpoint that this unlocks (S3c.4) follow.
