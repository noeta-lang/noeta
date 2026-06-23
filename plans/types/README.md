# Type-system track — inferred-static typing with explicit `dyn`

Status: **in-progress** (S0–S3b done)

This directory is the milestone-scale track that redirects the checker from **gradual / `Unknown`-tolerant** typing to **inferred-static** typing with an explicit `dyn` escape — Rust-like: every expression has an inferable static type, an un-inferable one is a *compile error* (never a silent `Unknown`), and `dyn`/`Any` is the only sanctioned dynamic boundary. The inference engine is **bidirectional checking + local inference, NOT Hindley–Milner** (subtyping via `dyn` widening, directional method resolution, and record width is load-bearing and defeats HM's unification core; bidirectional also decomposes cleanly into salsa queries). Rationale lives in the `type-system-direction` memory and `plans/deferred.md` (the superseded "checker hardening" / "static E0006" / "bounded generics" rows).

## Why this is safe to attempt

The corpus-migration audit (the recorded entry task) found the corpus overwhelmingly typeable already: ~70% of functions fully annotated, **0** cross-type reassignments, **0** `dyn`/duck-typing dependence, only ~9–14 loose *named* functions to annotate. And the runtime is *already literally the `dyn` path* — every dispatch is shape+name hashmap lookup, generics fully erased, no monomorphization. So this track adds a **rejecting static layer on top** and changes **no runtime representation and no dispatch**. "Soundness now, perf later" is structural: the only new runtime surface in the whole track is the `dyn` narrowing op (S6).

## Locked decisions

- **Engine:** bidirectional (synth/check + subsumption), not HM. Local inference for bodies.
- **Signatures:** required on every named `fn`/method (all params + return); **closures stay inferred**.
- **Bounds:** explicit `<T: Comparable>` at the declaration (Rust-style).
- **`dyn` narrowing:** `x.as<T>()` → `?T` (Option); `none` on mismatch. Implicit widening `T <: dyn`.
- **Out of scope (later perf track):** static dispatch ops, monomorphization, packed value types, SIMD. The runtime dynamic dispatch is untouched and becomes the marked `dyn` fallback.

## Oracle posture

The checker is **shared** by both backends (one `lang_check::check`), so any new static rejection is identical on both and the differential holds by construction — the only effect is that *more programs are rejected at compile time* and some corpus expectations shift from run→reject (tracked per slice). No new VM `Unsupported` surface (the `dyn` `as` op lands in both backends), so `--differential` stays at **0 skipped**. Baseline at the track's start: **conformance 113 / differential 107 matched, 0 skipped**.

## Slices

| Slice | Title | Status |
|---|---|---|
| S0 | Type lattice + `dyn` foundation (no verdict change) | **done** |
| S1 | Bidirectional engine rewrite at parity | **done** |
| S2 | Signature requirement + return checking (E0022) | **done** |
| S3a | Type the stdlib/method/prelude/index surface | **done** |
| S3b | Argument checking + flip concrete corpus cases | **done** |
| S3c.1 | Forward contextual propagation + map-literal inference | **done** |
| S3c.2 | Optional binding type annotations | todo |
| S3c.3 | Local backward-inference solver (reuse `Type::Var`) | todo |
| S3c.4 | Hard E0023 CannotInfer + conflict warning + finalize | todo |
| S4 | Explicit bounded generics, statically enforced (E0024) | todo |
| S5 | Trait coherence — orphan/overlap rules (E0025) | todo |
| S6 | `dyn` operations + checked narrowing (`x.as<T>()`) | todo |
| S8 | Declared union / intersection types ("closed `dyn`") | planned |
| S7 | Migration finalize + cleanup | todo |

Dependency order is strict-linear S0 → S3c.4, then S4 → S5 → S6 → S8 → S7 (S6 may land in parallel with S4/S5 — narrowing is independent of bounds; S8 is gated after S6, since a declared union is a *closed* `dyn` discriminated by the `x.as<T>()` narrowing). S3c grew from a single "hole-elimination" slice into four sub-slices once the corpus audit showed a naive hole→error flip is both unsound (rejects valid `return none`) and unfixable today (bindings carry no annotation); the four-way split — propagation, annotations, a local backward solver, then the `E0023` endpoint — is recorded in the session plan. Diagnostic codes are append-only; next free is **E0023** (reserved for S3c.4).
