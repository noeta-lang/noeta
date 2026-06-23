# Slice S3c.1 — Forward contextual propagation + map-literal element inference

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`). **Part of:** S3c (inference completion), the first of four sub-slices. **Depends on:** S3b (argument checking). **Determinism posture:** new static rejections from the shared checker → identical on both backends → differential unaffected (**112 / 0 skipped**). The two flipped/new corpus cases are compile errors (`exit 1`), agnostic to which phase produced them.

## Goal

Light up *checking*-mode propagation so the polymorphic literals (`none`, `some`/`Ok`/`Err`, empty `{}`) adopt their expected type and check their payloads against it, and give map literals real element inference. This is the pure-precision half of S3c: it catches mismatches gradual tolerance let through and resolves `return none` to its true `Option<T>`, *without* yet touching hole-elimination (the backward solver and `E0023` are S3c.3/.4). The naive "every hole errors" flip rejects valid `return none`; doing propagation first is what makes the later endpoint safe.

## What shipped

### Contextual propagation (`Checker::check`)
New check-mode arms, each adopting the expectation and checking payloads inward:
- `none` against `Option<T>` (`?T`) → `Option<T>` (carries no payload; simply absorbs the expectation instead of leaking a hole).
- empty `{}` against `Map<K, V>` → `Map<K, V>` (the map analogue of the existing empty-`[]`-against-`List<T>` arm).
- `some(x)` against `Option<T>` → check `x` against `T`.
- `Ok(x)` against `Result<T, E>` → check `x` against `T`; `Ok()` → unit payload subsumed against `T`.
- `Err(e)` against `Result<T, E>` → check `e` against `E`.

So `some("x")` against `Option<int>`, `Ok("x")` against `Result<int, _>`, and `Err(1)` against `Result<_, string>` are now caught — previously they deferred to a hole and passed.

### Map-literal element inference (`Checker::synth`)
`{k: v, …}` synthesizes `Map<K, V>` by unifying entries with the existing `unify_element` helper (`{"a": 1}` → `Map<string, int>`). Concretely-disagreeing values (`{"a": 1, "b": "two"}`) are a static `E0007` (the map analogue of a heterogeneous list), recovering as `Map<_, dyn>`. An empty `{}` leaves both slots an inference hole.

## Traps handled
- **`return none` / `return Ok(...)` in the corpus** stay green: every corpus polymorphic literal is already in a context (a declared return) that the new arms resolve, so nothing regresses — the propagation is what *prevents* the later strict flip from rejecting them.
- **`Ok()` with no argument** carries a unit payload (`Result<void, E>`), handled distinctly from `Ok(x)`.
- **Non-empty maps** are subsumed normally (only the empty-`{}` arm short-circuits in `check`); the synth path infers their elements.

## Files
- `crates/lang-check/src/lib.rs` — propagation arms in `check`; map element inference in `synth`.
- `crates/lang-check/src/tests.rs` — 5 new tests (option/result payload checking, `none` resolution, map inference, heterogeneous-map rejection).
- `tests/conformance/results/ok_payload_type_mismatch.lang`, `tests/conformance/types/map_heterogeneous_values.lang` — new negative corpus cases.

## Determinism / oracle posture
Conformance **118 passed**; differential **112 matched / 0 skipped / backends agree**. 50 checker unit tests (5 new), 15 lang-types tests; clippy + fmt clean.

## Definition of done — met
Polymorphic literals resolve against and are checked against their expectation; map literals infer their element types; two new corpus mismatches caught at compile time; suites green. The backward solver, optional binding annotations, and the `E0023` endpoint are S3c.2–.4.
