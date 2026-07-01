# Remaining performance work — the milestone-scale tracks (post-sweep)

**Status: planning (proposal for sign-off).** The perf *sweep* in [`README.md`](README.md) is
**complete** — P-COW (O(n²)→O(n) list append), P-IC (inline caches), P-GC (reframed: cycles collected
by the memory-management migration's backup trace + trial-deletion, no `gc-arena`), P-LAZY (streaming
`fs.open`), P-REUSE (Perceus-style reuse + drop insertion). P-PACK phases 0–3 also shipped (`@packed`
structs + E0038; flat packed values; flat `List<packed>`; `f32`) plus `bytes`/`to_bytes`/`from_bytes`.

What's left is not small deferred rows — it is a set of **milestone-scale tracks** the sweep cleared
the runway for. This doc inventories and sequences them so we pick deliberately rather than drift.

**Mandate (unchanged from the sweep):** every optimization ships with a **benchmark that validates the
gain** — a perf claim without a before/after number doesn't land. Every item is **invisible to
`RunResult`** (behaviour already correct), so the differential's `0 skipped / backends agree` gate holds
by construction and an optimization may land in **one backend first** (perf asymmetry, never behaviour).

## Inventory

| Tag | Item | Source | Blocked by | Size |
|---|---|---|---|---|
| **P-SIMD** | ~~Real SIMD kernels over the flat packed buffers~~ **→ tried, reverted (bench: 1.8×–9× *slower*). Blocked on a layout change (SoA / aligned buffers); see [`p-simd.md`](p-simd.md).** Scalar kernels already LLVM-autovectorized; the AoS/1-aligned `Vec<u8>` layout defeats manual SIMD. | P-PACK Phase 4 | **SoA or aligned buffers** | Medium |
| **P-SHARE** | Zero-copy borrow-share for isolate args — wire the built-but-unused I.3 `SharedRegion` into the real spawn path | isolates I.3/I.4 | `Rc<Shape>`→`Arc<Shape>` (a prereq slice) | Medium–large |
| **P-BITS** | Bit-level arc: Tier B (bitwise/shift ops on `int`) → Tier W (fixed-width ints) → Tier P (`Simd<T,N>` + const generics) | backlog (`plans/bitwise/`) | B: none · W: none · P: const generics | Large (3 tiers) |
| **P-MONO** | Monomorphic shape specialization + reflection cross-`dyn` element recovery (`type_of` recovers `List<int>`'s `int` after a `dyn` boundary once type args ride in shapes) | M1.8 / deferred "P2.9" | verify current type-arg-in-shape state (S4.5) | Medium |
| **P-POLISH** | Isolate real-path micro-items: `send` marshals only on a race today (already cheap); the stall-yield is a 100µs sleep-spin (could be a `Condvar`/park) | isolates I.4b/c | — | Small |

## Sequencing — value × independence, ascending in risk (the sweep's spine)

**1. P-SIMD first. → DONE, negative result (see [`p-simd.md`](p-simd.md)).** Was picked first for
highest value-per-risk (zero differential risk — a perf-only swap behind byte-identical scalar). The
oracle posture held perfectly (bit-identical, differential green), but the *perf* thesis failed: a
`wide`-crate `f32x8` swap benched **1.8×–9× slower** than the scalar kernels, which LLVM already
autovectorizes, because the AoS/1-aligned `Vec<u8>` layout forces scalar gather/scatter marshaling that
no SIMD width can pay for. Reverted. **The real P-SIMD win is now blocked on an SoA/aligned layout
change** — reclassified above. The `vec3_kernels` bench remains as the gate for that future attempt. So
the next *unblocked* track to pick is **P-BITS Tier B** or **P-SHARE** (per their rows).

**2. P-SHARE.** The item that makes I.3 pay off: real isolates currently **copy** args (`Wire`); a big
shared input (`isolate score(bigCorpus)` fanned to N workers) copies N times. The prereq is the one
real change — `Rc<Shape>`→`Arc<Shape>` (or shapes-by-`Module`-index in shared objects), because
`Value::shape()` clones a non-atomic `Rc`; once shapes are `Send`, wire `SharedRegion::promote`/
`free_all` (already built + miri-proven, I.3) into the real spawn path and add the big-input-no-copy
check. Structural (touches `lang-value`/`lang-object`/`lang-vm`) but in-crate and bounded; out-of-oracle
(the sandbox already agrees by copy≡borrow for immutable value types).

**3. P-BITS Tier B (bitwise/shift on `int`).** Cheap, high-value, self-contained: `& | ^ <<`/`>>`,
complement via `!` (Rust-style — avoids the `~`-is-concat clash), hex/bin/octal literals + `_`
separators, popcount-class intrinsics. New `Op::Binary` discriminants in the shared `apply_binary`, so
**both backends agree for free**, no value-repr change. Unblocks all flag/mask work. (Tier W fixed-width
ints and Tier P `Simd<T,N>` are larger, sequenced *after* and each with their own decision pass — Tier P
additionally needs const generics, which the S-track's bounded *type* generics don't yet provide.)

**4. P-MONO.** Reflection cross-`dyn` element recovery + monomorphic specialization. Depends on where
the type-system track left type-args-in-shapes (S4.5 gave `Type::Named` type args; needs a current-state
check before scoping). Deferred until its value is demonstrated by a workload.

**P-POLISH** rides along opportunistically inside whichever slice touches that code; not its own track.

## Per-track detail

### P-SIMD — SIMD kernels over flat packed buffers → **ready-to-execute slice doc: [`p-simd.md`](p-simd.md)**
- **Goal:** SIMD the bulk 3D-math kernels (`vec.add_all/sub_all/scale_all/dot_all/length_all` over a flat
  packed `List<Vec3>`, impls `lang_stdlib::vec3::*_buffers`), behind the *same* scalar results.
- **Toolchain note (found on inspection):** the build is **stable** (rustc 1.96.0), so portable
  `std::simd` (nightly-only) is unavailable — use the **`wide`** crate (stable portable SIMD), not
  `std::simd` as the older P-PACK Phase 4 plan assumed. Keeps the `unsafe`-forbidden rule intact.
- **Consumer/workload confirmed:** `vec.*_all` bulk ops + `tests/conformance/std/vec3_bulk.lang` already
  exist — a real consumer to bench (build a large `List<Vec3>`, time `add_all`/`dot_all`).
- **Oracle posture:** scalar semantics stay the spec; SIMD is internal to the kernels; the differential
  can't see it (both backends call the one kernel). One watch: keep f32 reduction order matching scalar.
- **Prereq:** ~~none~~ **an SoA or aligned-buffer layout change** — the flat AoS/1-aligned `Vec<u8>`
  buffer defeats manual SIMD (the scalar kernels are already autovectorized; `wide` benched 1.8×–9×
  *slower*, S2 reverted). A user-visible `Simd<T,N>` type is separately out of scope (P-BITS Tier P +
  const generics). **Outcome + full analysis: [`p-simd.md`](p-simd.md).**

### P-SHARE — zero-copy borrow-share for isolate args
- **Prereq slice:** `Rc<Shape>`→`Arc<Shape>` across `lang-object`/`lang-value`/`lang-vm` (or store shapes
  by `Module` index in shared objects so no per-object `Rc` crosses threads). Differential-neutral
  (single-thread behaviour identical), leak-oracle + miri must stay clean.
- **Then:** at `try_spawn_isolate_real`, promote the arg graph into a scope-owned `SharedRegion` once and
  hand each worker a borrowed (shared-tagged, no-op-rc) reference instead of a per-worker `Wire` copy;
  free the region at the structured join.
- **Gate:** a big-input-no-copy benchmark (one promotion-copy for N workers vs N copies) + existing CLI
  isolate integration tests stay green.

### P-BITS — bit-level arc (full design already at `plans/bitwise/README.md`)
- **Tier B:** operators + literals + intrinsics on `int` (i64). Shared `apply_binary`; hazards flagged in
  the design (don't lex `>>` as one token — compose in the parser; reuse the `Pipe` token for `|`).
- **Tier W:** fixed-width ints (`u8/u32/u64`, …) for *correct* masks (wraparound, zero-fill shift,
  exact-width popcount). Recommended repr: erase-to-i64 + type-directed masking. Four decisions to settle
  with the user first (which types, subtyping, repr, overflow policy).
- **Tier P:** `Simd<T,N>` — needs **const generics** (`<const N: int>`), a prerequisite the type-system
  track doesn't yet provide. Scalar-fallback semantics first (both backends agree), real SIMD a perf swap.

### P-MONO — monomorphic specialization + reflection cross-`dyn` recovery
- Type args ride in shapes so a materialized `List<int>` keeps `int` after a `dyn` boundary (`type_of`
  fidelity); monomorphic specialization is the reification path packed types began. Scope after a
  current-state check of the S4.5 type-arg-in-shape work; value demonstrated by a workload.

## Open decisions (to settle before starting)

1. **Which track first?** Recommendation: **P-SIMD** (unblocked, isolated, zero differential risk, the
   payoff P-PACK was built for). Alternatives: P-SHARE (makes I.3 pay off, but carries the `Rc→Arc`
   prereq) or P-BITS Tier B (cheapest, broadest unblock, but "flag/mask" value not perf-throughput).
2. **Is there a driving workload?** The mandate is bench-first. P-SIMD/P-SHARE each need a representative
   benchmark to justify — is there a target workload (a numerics kernel, a fan-out isolate job) to model,
   or do we synthesize one?
3. **P-BITS scope now:** Tier B only (cheap, high-value) or commit to the fixed-width (Tier W) arc?

## Verification (every slice, unchanged from the sweep)
- `lang test` conformance green; `lang test --differential` matched / **0 skipped** / backends agree.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --all --check` clean; leak oracle 0.
- The slice's **benchmark**, before/after, numbers recorded in the slice doc. Standard commit trailers.
