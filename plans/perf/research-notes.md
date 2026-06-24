# Memory-management research notes — inspiration for the perf track

Captured during the perf sweep so the survey isn't lost. The question that prompted it: *is
`gc-arena` (tracing) a generally-known faster model?* Answer: **no** — tracing-vs-refcounting is a
duality (Bacon, Cheng & Rajan, *"A Unified Theory of Garbage Collection"*, OOPSLA 2004), neither is
intrinsically faster, and `gc-arena` specifically is a simple **incremental, non-moving mark-sweep**
collector optimized for *safety/ergonomics in Rust* (it's the GC behind the Piccolo Lua VM), not for
throughput — it has no copying/bump-allocation win. So a migration would trade refcount traffic for
marking traffic with no guaranteed net gain, while *breaking* deterministic `__destruct` (tracing
can't give prompt/ordered finalization — which is why the old plan only put destructor-free classes
on the tracing path, i.e. a hybrid, i.e. more machinery).

## Our design point

Immutable-by-default, value-semantics language; runtime values are refcounted (`Rc` in the
tree-walker, a refcounted heap in the VM); **shared-nothing per isolate** (so RC is already
non-atomic); **deterministic `__destruct`** is language-observable. This is *precisely* the design
point of Koka / Lean 4 / Roc — which means their research line is the most directly applicable, and
crucially it is **compatible with deterministic destruction** (precise RC reclaims at last use),
whereas tracing fights it.

## The line that fits best: precise RC + reuse analysis

The COW list-append we just shipped (mutate in place when `Rc` is uniquely owned) is a *manual,
runtime-checked* instance of **reuse analysis**. This research makes it a systematic compiler pass.

- **Perceus** — Reinking, Xie, de Moura, Leijen, *"Perceus: Garbage-Free Reference Counting with
  Reuse"*, PLDI 2021. Compiler inserts precise `dup`/`drop` (free at last use, not scope end) and
  does **reuse analysis**: a uniquely-owned value consumed + a same-shape value constructed ⇒ reuse
  the allocation in place. Generalizes our COW to *every* constructor, not just `~`. (Koka.)
- **Counting Immutable Beans** — Ullrich & de Moura, IFL 2019. The Lean 4 precursor; the reuse-token
  idea ("refcount==1 ⇒ reuse the cell") is exactly our `Rc::get_mut` trick, but inserted *statically*
  by the compiler instead of checked per-op at runtime. Shipped in a real perf-sensitive system.
- **Frame-Limited Reuse** (Lorenzen & Leijen, ICFP 2021) and **FP²: Fully-in-Place Functional
  Programming** (Lorenzen, Leijen, Swierstra, ICFP 2023) — formalize *when* an algorithm runs fully
  in place and keep reuse from accidentally extending lifetimes. The theory behind which accumulator
  patterns can be made allocation-free in general.

Why it matters here: Perceus-style reclamation is **prompt and deterministic** (last-use), so it
*preserves* `__destruct` ordering. The cutting-edge direction is also the constraint-compatible one.

## Compile-time uniqueness (skip the runtime check entirely)

- **Roc** — opportunistic in-place mutation. Also immutable + RC, but infers uniqueness at *compile
  time* (a morphic-style solver) and mutates in place when safe, so it doesn't even pay the runtime
  `strong_count == 1` check we currently do. Closest cousin in spirit; study how it exposes this
  without surfacing mutability.
- **ASAP** — Raphaël Proust, *"ASAP: As Static As Possible memory management"*, Cambridge PhD ~2017.
  Liveness/region analysis frees memory with no RC and no tracing. A mental model for how far static
  analysis can go; more radical than we need.

## If the real goal is cycles (not throughput): RC + backup trace

- **LXR** — Zhao, Blackburn, McKinley, *"Low-Latency, High-Throughput Garbage Collection"*, PLDI
  2022. An **RC backbone** (promptness, low overhead) with an **occasional backup tracing pass**
  (on Immix's cache-friendly mark-region structure) to collect cycles. This solves our destructor-
  free-cycle leak *without* abandoning RC's promptness — the principled version of the hybrid the
  gc-arena plan was groping toward, and it keeps deterministic finalization on the RC path.
- **Immix** — Blackburn & McKinley, PLDI 2008. The mark-region substrate LXR and others build on.
  Context, not a direct adopt.

## Genuinely different points in the space (know them, probably don't adopt)

- **Vale generational references** (Evan Ovadia) — neither RC nor tracing: a generation number per
  allocation, each pointer remembers its expected generation, checked on deref. Trades a load+compare
  per dereference for zero GC and zero refcount traffic. *Mostly blog/design-doc documented, not
  peer-reviewed — confidence caveat.*
- **Biased Reference Counting** — Choi, Shull, Torrellas, PACT 2018. Splits the count into a
  non-atomic owner part + atomic shared part. **We already get the main benefit for free** (shared-
  nothing isolates ⇒ non-atomic `Rc`). Cite as "why our RC is already cheap," not new work.

## Infrastructure, if we ever go full GC

- **MMTk** (Blackburn et al.) — a pluggable, **Rust** memory-management framework designed to bind
  into language runtimes. The realistic path if we ever genuinely need a high-performance collector —
  far more credible on throughput than gc-arena (which optimizes safety/ergonomics).

## Recommendation for this track

Highest-leverage inspiration is the **Perceus / Lean-beans / Roc** cluster: make reuse analysis a
*compiler pass* rather than a per-op runtime check. Our manual COW append is the proof-of-concept;
the research shows how to (a) hoist the uniqueness decision to compile time (Roc) and (b) generalize
reuse to every constructor (Perceus/FP²). This is a milestone-scale track, and it's **adjacent to the
type system we just finished** — the static analysis has the type/ownership information it needs.

**So P-GC is reframed:** *not* "migrate to gc-arena." Instead, the future work is "evaluate
**Perceus-style static reuse** (compile-time generalization of COW) + **LXR-style RC-plus-backup-
trace** (cycle collection without losing deterministic destruction)." Both are measurement-gated:
do them only when a benchmark shows refcount traffic dominating (for static reuse) or cycle leaks
mattering in a real workload (for the backup trace). The near-term concrete step — **VM-side COW** —
needs none of this: it's the same uniqueness check on the VM heap list that the eval side already
does, closing the eval-O(n)/VM-O(n²) asymmetry.

> Confidence: venues/years above are recalled to the best of knowledge (cutoff early 2026); the
> Perceus/Beans/FP²/LXR/Immix/Biased-RC/MMTk citations are high-confidence on substance and venue,
> Vale and ASAP are flagged as less formally published. Verify exact citations before quoting.
