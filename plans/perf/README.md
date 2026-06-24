# Performance sweep — the deferred perf items

The post-P2.7 arc (abstract-kind-types, type_of-kinds, semantic-roles, structured-args,
generic-derives) is **complete**, and the standing directive is the **perf-related deferred
items** in [`plans/deferred.md`](../deferred.md). This directory is that sweep: one README
(the ordering + rationale, below) and one slice doc per item.

**Mandate (decided with the user):** try *every* optimization, and for each one **build a
benchmark that validates the gain**. A perf claim without a before/after number doesn't ship.

## Posture

Every item here is **invisible to `RunResult`** — behavior (stdout / exit code) is already
correct, so the differential's `0 skipped / backends agree` gate is unaffected by construction.
That gives us freedom: we can land an optimization in **one backend first** (a temporary
perf asymmetry, never a behavior asymmetry) and the differential stays green.

Benchmarks live in two places, because the two backends have different hot paths:
- **VM** (`crates/lang-vm/benches/vm.rs`) — the existing M2.0 baseline (dispatch / property /
  allocation). Extended here.
- **Eval / tree-walker** (`crates/lang-eval/benches/eval.rs`) — added in Phase 0, because
  P-COW lands on the tree-walker first and the VM benches can't see it.

Both use criterion. For asymptotic claims (P-COW's O(n²)→O(n)) we use **parameterized**
benches over input size, so the *scaling* is visible, not just a constant-factor delta.

## The items

| Tag | Item | Source | Nature |
|---|---|---|---|
| P-COW | Copy-on-write / unique-owner in-place list append (`acc ~= [x]` is O(n²)) | L1 | Algorithmic |
| P-IC | Inline caches for member access + trait-method call sites | M1.4, M1.8 | Micro-opt (bench-gated) |
| P-GC | `gc-arena` tracing path for `__destruct`-free classes | M1.6 | Structural (heap model) |
| P-LAZY | Lazy real-disk reads behind the `fs.open` handle | M2.5 | Niche I/O |

`P-PACK` (monomorphic specialization + packed value types) is **milestone-scale** (the M2
"packed value types" track), not a single deferred item. The inferred-static type system it was
gated on is now complete, so it is *unblocked* — but it is a next milestone, the thing this
sweep clears the runway for, not part of the sweep. It subsumes the reflection cross-`dyn`
element-recovery ("P2.9"). Out of scope here.

## Ordering & rationale

The spine is **value × independence, ascending in risk**: the algorithmic bug first (biggest
win, zero behavioral risk), then the ready-to-measure micro-opt, then the structural heap
change that absorbs the deferred half of P-COW, then the niche I/O item.

### Phase 0 — baseline & bench infrastructure → [`phase-0-benchmarks.md`](phase-0-benchmarks.md)
Run the existing benches, record numbers, add the cases the later slices need (an accumulator
parameterized over n; a member-dispatch program) and an eval-side bench harness. Every item
below claims a win; we want before/after numbers to prove it. Cheap, and it makes the rest
measurable.

### 1. P-COW — list-append copy-on-write → [`p-cow-list-append.md`](p-cow-list-append.md)
*Highest value/effort ratio, first.* The only item that is an actual **complexity bug**
(O(n²)→O(n)), not a constant-factor micro-opt — so it needs no benchmark to *justify*, only to
*quantify*. Observably invisible (same immutable semantics), so the differential can't break.
**Split:** the tree-walker side is self-contained and ships first as a clean commit; the VM
side needs uniqueness info from the heap allocator and **folds into P-GC** (#3) rather than
blocking here. Temporary asymmetry (eval O(n), VM O(n²)) is invisible to the differential.

### 2. P-IC — inline caches → [`p-ic-inline-caches.md`](p-ic-inline-caches.md)
*The most "ready" bench-gated item.* The M2.0 harness was built anticipating this
(`property_access` + `dispatch_fib` measure exactly these sites), so the gate is already
satisfiable and we get hard numbers. Self-contained inside the VM, no cross-backend surface
(eval has no call sites to cache), so no differential risk.

### 3. P-GC — `gc-arena` tracing + VM-side COW → [`p-gc-tracing.md`](p-gc-tracing.md)
*Heavier, structural, so it follows the cheap wins.* Touches the ownership/heap model and is
the riskiest. The **VM half of P-COW depends on the allocator exposing uniqueness**, so doing
GC here lets that change land once and carry VM-side COW on top of it — rather than building
throwaway uniqueness tracking in #1.

### 4. P-LAZY — lazy `fs.open` reads → [`p-lazy-fs-open.md`](p-lazy-fs-open.md)
*Lowest priority, demand-driven.* Only matters for files too large to buffer whole; the surface
is already final. Park it until a real workload hits it.

## Verification (every slice)
- `cargo run -q -p lang-cli -- test` → conformance green.
- `cargo run -q -p lang-cli -- test --differential` → matched / **0 skipped** / backends agree.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --all --check` → clean.
- The slice's **benchmark**, run before and after, with the numbers recorded in the slice doc.
- Branch `types-inferred-static`; standard commit trailers.
