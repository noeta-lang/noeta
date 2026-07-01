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
| **P-SIMD** | Real SIMD kernels (`std::simd`) over the flat packed buffers, behind byte-identical scalar 3D-math (`vec3`/`quat`/`List<f32>`) | P-PACK Phase 4 | — (unblocked) | Medium |
| **P-SHARE** | Zero-copy borrow-share for isolate args — wire the built-but-unused I.3 `SharedRegion` into the real spawn path | isolates I.3/I.4 | `Rc<Shape>`→`Arc<Shape>` (a prereq slice) | Medium–large |
| **P-BITS** | Bit-level arc: Tier B (bitwise/shift ops on `int`) → Tier W (fixed-width ints) → Tier P (`Simd<T,N>` + const generics) | backlog (`plans/bitwise/`) | B: none · W: none · P: const generics | Large (3 tiers) |
| **P-MONO** | Monomorphic shape specialization + reflection cross-`dyn` element recovery (`type_of` recovers `List<int>`'s `int` after a `dyn` boundary once type args ride in shapes) | M1.8 / deferred "P2.9" | verify current type-arg-in-shape state (S4.5) | Medium |
| **P-POLISH** | Isolate real-path micro-items: `send` marshals only on a race today (already cheap); the stall-yield is a 100µs sleep-spin (could be a `Condvar`/park) | isolates I.4b/c | — | Small |

## Sequencing — value × independence, ascending in risk (the sweep's spine)

**1. P-SIMD first.** Highest value-per-risk of the unblocked items: the throughput payoff P-PACK was
built *for*, fully isolated (the kernels are internal Rust over the flat buffers `vec3`/`quat`/`math`
already own), and **zero differential risk** — real SIMD is a perf-only swap behind the existing
byte-identical scalar semantics, so the oracle can't break. Needs no new language surface (no user
`Simd<T,N>` — that's P-BITS Tier P). A batch op over a large `List<Vec3>` is the natural bench.

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

### P-SIMD — SIMD kernels over flat packed buffers
- **Goal:** `std::simd` (portable-SIMD, no FFI seam — recommended in the P-PACK Phase 4 plan) for the
  hot 3D-math kernels over contiguous `List<f32>`/`List<Vec3>` (batch add/scale/dot/normalize), behind
  the *same* scalar results both backends produce today.
- **Oracle posture:** scalar semantics stay the spec; SIMD is an internal Rust detail of the `lang-stdlib`
  registry kernels. Differential can't see it. Gate on a criterion bench (batch op over n, parameterized).
- **Prereq:** none for the internal kernels. A *user-visible* `Simd<T,N>` type is explicitly out of scope
  here (that's P-BITS Tier P + const generics).
- **First slice:** pick one kernel (e.g. `List<Vec3>` component-wise add), add a parameterized bench,
  swap its inner loop to `std::simd`, record before/after.

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
