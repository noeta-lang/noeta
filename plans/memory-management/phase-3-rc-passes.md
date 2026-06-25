# Phase 3 — Precise RC on the IR

The heart of the migration: the precise-reference-counting pipeline as **passes over the Core IR**,
filling the `dup`/`drop`/reuse slots reserved in Phase 1. Because both backends consume the *same*
annotated IR, prompt reclamation lands in both at the same points by construction. Reclamation is still
behavior-invisible here (destructors stay globals-only until Phase 4), so this phase is differential-green.

## 3.1 Last-use / liveness pass (`lang-ir-passes`)

A structured **backward** dataflow over the Core IR computing, for every named value, its last-use
point(s). ANF makes this clean — each value has one definition and a set of uses; last-use is the final
use on each path.

- **Loops:** a value used across a back-edge is live to the loop exit (the positional subtlety
  P-REUSE encoded by hand falls out of the dataflow). **Branches:** a value used in one arm dies at
  that arm's end on that path. **`?`/`return`/`break`/`continue`:** abnormal exits move-out or abandon
  live values; record move-out (escapes, not dropped) vs in-scope death.
- **Conservative "never too early"** (README safety invariant): where flow makes last-use uncertain,
  *omit* the drop (value lives to scope end, reclaimed by teardown). The **static-≤-dynamic property
  test** (Phase 0/this phase) machine-checks that no computed death precedes the real one.

## 3.2 `dup`/`drop` insertion pass

Using last-use, insert RC ops on the IR:

- A **non-last** use of an owned value inserts a `dup` (retain); the **last** use is a **move**
  (ownership transfers, no retain) — this is the general form of P-REUSE's "evaluate directly into the
  register" and "no lingering receiver temp." A value whose last use is *not* a consumer gets an
  explicit `drop` at its death point.
- Drops carry **destructor-relevance** (from the type facts): a drop of a `destruct`-bearing type lowers
  to the destructor-running release; others to a plain release. (Phase 4 turns the relevance on for all
  scopes; here it still only *fires* for globals, but the annotation is computed now.)
- The pass is **idempotent-safe with teardown**: an omitted/late drop is caught by scope/teardown
  release; an inserted drop clears its slot so teardown never double-frees (the `Op::Drop` semantics we
  already have).

## 3.3 Backend lowering of RC ops

- **VM:** `drop` → `Op::Drop`; last-use move → consuming move (no retain); and a **reuse-aware register
  allocator** that, given the now-explicit last-use intervals, reuses register slots (non-overlapping
  lives share a register → fused release on overwrite, zero extra ops). This **subsumes and retires**
  P-REUSE's `declare_local` no-Move special case, the `drop_receivers` gated receiver drop, and the
  monotonic allocator. `Chunk.num_registers` shrinks.
- **Tree-walker:** `dup`/`drop` become explicit `Rc` clone / drop-with-last-ref-check at the IR points.
  Memory is still freed by Rust `Rc`, but the *drop points* are now the IR's — aligning eval's
  reclamation timing with the VM's. (Destructor *firing* at those points is Phase 4.)

## 3.4 Reuse-token threading (foundation; full reuse is Phase 5)

The drop-insertion pass also records, where a uniquely-owned value's `drop` is immediately followed by
a same-shape constructor, a **reuse token** linking them. This phase *computes and represents* tokens
(and may light up the record/list cases already covered by P-REUSE to prove the path); Phase 5
generalizes consumption to every constructor. Reuse is always backed by the runtime `RC == 1` check —
the token says *where to try*, the check says *whether it's safe this run*.

## Verification gate

- Conformance + **differential 0 skipped / agree** (reclamation timing changed, destructor output not).
- **Static-≤-dynamic last-use property test** green over the corpus.
- Leak oracle residency `== 0`, now reached *promptly* (mid-run), not at teardown — the **peak
  residency drop is the headline metric** (record vs Phase-0 baseline).
- miri on the VM allocator + `dup`/`drop` lowering + a branchy/loopy/closure stress corpus (no early/
  double free). clippy + fmt clean.
- P-REUSE's targeted machinery removed; its conformance tests pass via the general path.
- Bench: allocation churn + peak residency improve; dispatch/property within noise.
