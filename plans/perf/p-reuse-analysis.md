# P-REUSE — compile-time reuse analysis (Perceus/Roc-style), prototype + benchmark

Status: **PROTOTYPE COMPLETE + DROP INSERTION DONE.** The prototype measured both axes and identified
**drop insertion** as the gating prerequisite; the follow-up below then *implemented* targeted drop
insertion, unlocking reuse for local and read-update accumulators (the idiomatic cases) at **~3×**.
Jump to **"Drop insertion (follow-up)"** for that work. The original prototype writeup follows.

Status (prototype): **both axes measured.** Generalizes the hand-rolled COW list append
(`p-cow-list-append.md`) to a second constructor — **record functional-update** `acc = Type { ...acc,
… }` — and prototypes the **compile-time uniqueness hoist** (the static-reuse path). Both backends;
benchmarked. The headline result is a clear answer to "what gives the highest performance," including
a hard backend constraint the prototype surfaced. Conformance 222 / differential 215 / 0 skipped /
agree; clippy + fmt clean; the in-place mutation paths are miri-validated.

## The two axes (what the user asked to compare)

1. **Generalize reuse to a new constructor (Perceus/FP²).** Record update `Type { ...acc, f: v }`
   today allocates a fresh object and copies *every* field from the spread base. When the base is
   uniquely owned, mutate it in place instead — overwrite only the changed slots, the other fields'
   references transfer from base to result. This is the COW list-append idea applied to records.
2. **Hoist the uniqueness decision to compile time (Roc).** The COW/runtime path checks
   `refcount() == 1` at the construct. If the compiler can *prove* the accumulator is uniquely owned
   (a linearity analysis), the runtime check is redundant and can be elided.

## What shipped

- **`Op::MakeRecordInPlace { dst, shape, named, base, check, span }`** + **`ReuseCheck::{Runtime,
  Static}`** (lang-bytecode). The record analogue of `ConcatInPlace`: `base` is the consumed
  accumulator (its register cleared without release, transferring the single reference). `Runtime`
  checks `same-shape && refcount()==1` and reuses or copies; `Static` elides the refcount check
  (uniqueness proven) and reuses, guarding shape defensively. Falls back to a full copying build
  exactly like `MakeRecord` when it cannot reuse.
- **Compiler self-update detection** (`record_self_update`) + the **linearity analysis**
  (`linear_record_accumulators`): a global accumulator is `Static`-eligible iff it appears *only* as
  the consumed spread base of its own updates, read at most *after* its last update (`last_update_index`).
- **`ReuseMode::{Off, Runtime, Static}`** + `compile_with_options` — a benchmark ceiling letting the
  bench compile the *same* source three ways. Production default is `Static` (gated by the analysis).
- **Tree-walker reuse** (`try_record_reuse`, lang-eval): `Rc::get_mut` the accumulator's field map
  when uniquely owned (peeked via `Rc::strong_count`), else copy — the same shared⇒copy / unique⇒mutate
  invariant. A `record_reuse` knob (bench-only) toggles it.
- Conformance: `classes/record_update_static.lang` (static path fires, debug-assert-validated),
  `record_update_reuse.lang` (read-update → runtime copy fallback), `record_update_aliasing.lang`
  (aliased base → copy, preserving the alias). VM unit test `record_update_reuse_paths` (miri).

## The central finding: reuse is gated by **drop insertion**, which the register VM lacks

The prototype surfaced a hard constraint that *is* the answer to "how far does this go." The register
machine **frees no temporary register until the frame ends**, and reads **retain**:

- `acc.field` lowers to `Move`/`LoadField` into a temp that **retains** the accumulator and lingers
  to frame end. So *any read* of the accumulator leaves a second reference — it is no longer uniquely
  owned at a later construct.
- `declare_local` binds a local with a **retaining `Move`** from the initializer's temp — so a *local*
  accumulator is born at refcount 2 (the temp lingers) and never reuses at all.
- A **global** accumulator is stored via the **consuming** `StoreGlobal` (refcount 1) and stays unique
  *as long as it is not read between updates*. This is exactly why the COW list append works (a global,
  and `acc = acc ~ [i]` never reads `acc`).

So on this backend, refcount-based reuse fires **only for a global blind-overwrite accumulator** —
the "fully in place" (FP²) pattern. Generalizing to locals or to updates that *read* the accumulator
(`acc = T { ...acc, x: acc.x + 1 }`) requires **precise last-use drop insertion** — freeing the dead
temporaries at last use — which is the actual core of the Perceus pass. The tree-walker gets this for
free: Rust drops the read's temporary promptly, so **eval reuses even read-updates** that the VM
cannot. The prototype thus quantifies what is achievable *without* drop insertion and identifies drop
insertion as the gating prerequisite for going further. That is a milestone-scale pass; not in scope
here (flagged, not silently dropped — see `confirm-before-deferring-scope`).

## Benchmarks (validate the gains)

Blind-overwrite accumulator, an 8-field record, one field overwritten per iteration, read after the
loop. Same source compiled three ways. Quick criterion settings (warm-up 0.5 s, 2 s, 30 samples).

**VM** (`vm_record_update`):

| n | off (copy) | runtime (reuse) | static (check elided) |
|---|------------|-----------------|-----------------------|
| 1000 | 162.6 µs | 69.8 µs | 69.5 µs |
| 2000 | 330.7 µs | 138.9 µs | 138.8 µs |
| 4000 | 654.4 µs | 277.3 µs | 273.6 µs |
| 8000 | 1.341 ms | 559.0 µs | 549.7 µs |

**Tree-walker** (`eval_record_update`, off vs on):

| n | off (copy) | on (reuse) | speedup |
|---|------------|------------|---------|
| 1000 | 602 µs | 194 µs | 3.1× |
| 8000 | 4.849 ms | 1.501 ms | 3.2× |

## Interpretation — the answer

- **Axis 1 (generalize reuse to records): the win.** VM **~2.4×** on the construct, eval **~3.1×**.
  This is a *constant-factor* win (a record has fixed width, so it is O(n·fields)→O(n), scaling with
  field count — not the asymptotic O(n²)→O(n) the *growing* list gave). The more fields, the larger
  the win. Avoiding the allocation + per-field copy is the real lever.
- **Axis 2 (hoist uniqueness to compile time): negligible.** Runtime vs static is within noise
  (~0–2%). The runtime `refcount() == 1` check is a single cheap branch dwarfed by the slot writes
  and dispatch. **Compile-time uniqueness hoisting is not worth its analysis complexity on this VM**
  — its only payoff would come bundled with drop insertion (which changes *what* can reuse, not the
  per-op check cost).
- **Recommendation.** Ship the runtime-checked record reuse (axis 1) as the generalization of the
  COW; treat the static path as a measured-but-marginal curiosity. The high-value next step is **not**
  more static analysis but **drop insertion** (free dead temporaries at last use) — it is what unlocks
  reuse for locals and read-updates, i.e. the cases that actually occur in idiomatic code. **(Done —
  see "Drop insertion (follow-up)" below; ~3× on the read-update accumulator.)**

## Drop insertion (follow-up): unlocking locals + read-updates

The prototype found reuse fired only for a global blind-overwrite accumulator — because the register
machine frees no temporary until frame end, so a *local* accumulator (born via a retaining
declaration `Move`) and any *read* of the accumulator (`acc.field` retains it into a lingering temp)
stay above refcount 1. The fix is **targeted drop insertion** for exactly those compiler-generated
single-use temporaries — provably dead by construction, so no CFG-liveness pass is needed.

- **Step A — no declaration `Move`.** A fresh non-celled `mut` local is now evaluated *directly into
  its own register* (`binding`), instead of into a temp then `Move`d (retained) into the local. That
  retaining `Move` left the value at refcount 2 forever (the temp lingers). Now a local accumulator
  is refcount 1 → local self-update reuse re-enabled (`MakeRecordInPlace` in the local branch).
- **Step B — `Op::Drop { reg }`** (release + clear to `unit`, idempotent with `set_reg`/teardown so
  nothing double-frees). `member()` emits it after a `LoadField` to free the receiver temp — but only
  while lowering a **reuse construct's** field initializers (`drop_receivers` flag, set in
  `eval_in_place_named`). So `acc = T { ...acc, x: acc.x + 1 }` frees the `acc.x` read's temp before
  the in-place mutation (reuse fires), while ordinary field reads everywhere else emit no `Drop` and
  pay nothing — no regression. (A general drop pass over *all* temps cost ~+4% on field-read loops;
  gating it to reuse constructs removed that.)

`Op::Drop` is miri-validated (no double-free); conformance `record_update_drop_insertion.lang` (local
+ read-update, heap field) + VM unit test `record_update_reuse_with_self_read`. Conformance 223 /
differential agrees / 0-skipped.

**Benchmark — read-update accumulator (the unlocked case), `vm_record_update_read`** (local, inside a
function, `acc = Wide { ...acc, f0: acc.f0 + 1 }`, 8 fields; off = copy, runtime = reuse):

| n | off (copy) | runtime (reuse) | speedup |
|---|------------|-----------------|---------|
| 1000 | 154 µs | 57.6 µs | 2.7× |
| 2000 | 311 µs | 118.9 µs | 2.6× |
| 4000 | 611 µs | 220.7 µs | 2.8× |
| 8000 | 1.269 ms | 403.9 µs | 3.1× |

This is the idiomatic accumulator (a local, reading itself) the prototype found blocked — now reusing
at ~3×. Field-read regression check (gated `Drop`): `vm/property_access` 608 µs → 565 µs (within
noise, no regression); `dispatch_fib` (no field reads) unchanged.

**Takeaway.** Targeted drop insertion — not general CFG liveness — was enough to unlock the cases that
matter, because the reuse-blocking temporaries are compiler-generated and provably single-use. The
remaining gap (a fully general last-use pass for arbitrary user code) stays milestone-scale and
unmotivated by these numbers: the targeted version already captures the accumulator-reuse win. Axis 2
(static check elision) remains negligible — read-updates ride the runtime path and reuse just as well.

## Files

- `crates/lang-bytecode/src/lib.rs` — `ReuseCheck`, `Op::MakeRecordInPlace`, `Op::Drop` + disasm.
- `crates/lang-vm/src/lib.rs` — dispatch arm (static/runtime/copy paths); unit test (miri).
- `crates/lang-compiler/src/lib.rs` — `ReuseMode`/`compile_with_options`, `record_self_update`,
  `linear_record_accumulators` + the positional last-update refinement, both local & global
  self-update lowering; **drop insertion** — `binding` (no declaration `Move`) + `member()`'s gated
  receiver `Drop` (`drop_receivers`, set in `eval_in_place_named`).
- `crates/lang-eval/src/lib.rs` — `try_record_reuse`, `record_reuse` knob, `run_without_record_reuse`.
- Benches: `crates/lang-vm/benches/vm.rs` (`vm_record_update`, `vm_record_update_read`),
  `crates/lang-eval/benches/eval.rs` (`eval_record_update`).
- Conformance: `tests/conformance/classes/record_update_{static,reuse,aliasing,drop_insertion}.lang`.
