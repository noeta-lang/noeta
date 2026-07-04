# Where the PHP gap still lives — post-P-VMT investigation (2026-07-03)

After the P-VMT arc (S0–S5) closed part of the scalar/loop/call gap (loop 10M −21%, fib −9%), this
is a disassembly-driven look at *what still dominates* the remaining 5–250× gap to PHP 8.4, and the
next slices that would close it. Evidence is `lang dump <file>` on the `scratch-bench/` programs.

## Finding 1 — top-level bindings & global functions go through a string-keyed `HashMap` — ✅ DONE (`ecee093`, P-VMT-GSLOT)

**Fixed.** The compiler now assigns each global a dense slot (`GlobalId` + `Module::global_names`)
and the VM stores globals in a `Vec<Option<Value>>` the three ops index directly — no name hashing
(PHP's CV-slot model). `None` marks unbound (E0005 preserved); the slot→name table is used only for
diagnostics/disassembly (snapshots byte-identical), and the cross-thread isolate seeding keys on the
slot id (shared `Arc<Module>` → slots line up). **b_loop 1279 → 548 ms (2.3×), empty 2M loop 124 → 65
ms (1.9×), fib(32) 592 → 502 ms (1.18×)**; new `vm_dispatch/global_loop` bench. Original analysis
below. *(The deeper refinement — register-allocating uncaptured top-level locals so `i`/`total` avoid
the global array entirely — remains open.)*

---



`Vm.globals` is a `HashMap<String, Value>`. Every top-level `mut`/`let` binding and every top-level
`fn` name lives there, so each use hashes and probes a string.

- **`b_loop` (loop 10M, ~1250 ms).** `i` and `total` are top-level `mut` → the 16-op inner loop does
  **6 string-keyed map ops per iteration**: `LoadGlobal "i"` ×3, `LoadGlobal "total"`, `StoreGlobal
  "total"`, `StoreGlobal "i"`. That is **60M hash+probe ops** for the run. PHP compiles `$i`/`$total`
  to array-indexed CV slots — no hashing.
- **`b_fib` (fib(32), ~610 ms).** Each call does **2× `LoadGlobal "fib"`** (ops 7, 12) to resolve the
  recursive callee — ~14M string lookups for fib(32), on top of the call machinery.

**Fix — global slot indexing.** The compiler already knows every global name (`module_globals`).
Assign each a dense `GlobalId(u32)` at compile time; store globals in a `Vec<Value>` (or `Vec<Option>`
for the unbound check) indexed by id; `LoadGlobal`/`StoreGlobal`/`TakeGlobal` carry the id, not a
`NameId`. Keep a `name → id` map only for the cold dynamic paths (`invoke`, reflection). This removes
all string hashing from top-level loops and every global-function call — mirrors PHP's CV slots. It is
the single broadest interpreter win left. Moderate blast radius (bytecode + compiler + VM), but the
ops already exist — it is a field-type swap plus a `Vec` in the VM.

**Later refinement:** register-allocate top-level locals that no nested `fn` captures (turn `i`/`total`
into pure frame registers, no global array at all). Bigger; do slot-indexing first.

### Spike result (investigated + benchmarked — DEFERRED, low ROI)

Spiked as a post-compile bytecode pass on proto 0 (promote globals used only in `main`, first-referenced
by a store, to fresh registers; `LoadGlobal→Move`, `StoreGlobal→Drop+Move+Drop`, `TakeGlobal→Move+Drop`,
refcount-matched exactly, final value dropped before `Halt`). Findings:

- **Ceiling is small.** The gap between an all-global loop and the identical function-local loop —
  which is exactly what full promotion would close — is only **~5% in the interpreter** (`global_loop`
  44ms vs `loop_sum` 39ms/1M; global-slot-indexing already made a global access nearly a bare `Vec`
  index) and **~30% in the JIT** (`global_jit` ~9.2ms vs `loop_jit` ~6.3ms/1M). So the prize is narrow:
  top-level scripting loops that also get hot enough to JIT.
- **The cheap approach is a net LOSS.** The bytecode pass made it *slower* (interp 44→60ms, JIT
  9.2→9.4ms): it turns `LoadGlobal→temp` into `Move reg→temp` (no win — the `Binary` still reads the
  temp, not the promoted register), and `StoreGlobal` (1 op) into `Drop+Move+Drop` (3 ops) — **14
  ops/iter vs the global 10 and the frontend-local 11.** A peephole can't restructure the redundant
  load-into-temp that a frontend register allocator simply never emits; reaching the ceiling would mean
  re-running copy-propagation + coalescing on the promoted code. **So this must be a frontend change**
  (allocate the local as a register from the start, reads hit it directly), which needs `main` to carry
  a base scope (`declare_local` panics with none) — real surgery.
- **Teardown-order hazard is real and observable.** Adversarial test (two top-level `destruct`-bearing
  objects, one promoted/`main`-only, one kept global because a `fn` reads it): destruction order
  diverges — baseline `drop b; drop g; drop a`, promoted `drop b; drop a; drop g` (`g` moves to the
  end). Refcount matching was exact (leak oracle 0) and the corpus differential stayed green (no corpus
  program hits the mix), but a sound version **must** gate promotion to non-destructor-bearing bindings
  (needs type info from the checker) or promote all-or-nothing per teardown group.

**Recommendation: defer.** Marginal interpreter win, a narrow JIT win, and the sound version needs
frontend surgery + destructor-order gating. Better ROI elsewhere. If revisited, do it in the frontend,
not as a bytecode pass.

## Finding 2 — read-modify-write on a collection copies instead of reusing (the 250× outlier) — ✅ DONE (`094cc1a`, P-VMT-RMW)

**Fixed.** `insert_drops` runs before `thread_reuse`, so reading the receiver earlier in the block
puts a `DropVar` between the `m.set(...)` self-update and its `m = %t` rebind; the pass required the
rebind at exactly `stmts[i+1]`, so the drop denied the reuse token. Made the pairing tolerant of
intervening drops (`rebinds_temp_after_drops`, applied to all four self-update shapes). Sound: the
reuse op consumes the receiver at the op, so a later drop hits a moved-out unit slot, and the runtime
`refcount==1` guard still copies under aliasing. **O(n²) → O(n):** new `vm_map_rmw` bench 28× (n=1000)
→ 37× (n=8000); wordcount ~2770 ms → ~84 ms (33×), from ~250× behind PHP to ~7.6×. Original analysis
below.

---



The `m[k] = f(m[k])` idiom — counters, histograms, accumulation — is extremely common and today is
**O(n²)** whenever the collection is *read* earlier in the same iteration.

- **`b_wordcount_fn` (~2770 ms, ~250× behind PHP).** `prev = if m.has(key) then m[key] else 0; m[key]
  = prev + 1`. The update lowers to `CallMethod r0.set(r2, r4)` **with no `[reuse]` marker** (op 23) —
  it builds a fresh map, drops the old one, and moves the copy in (ops 24–27). With ~500 live keys ×
  200k iterations that is ~100M entry-copies.
- **`c_assoc_fn` (~72 ms, fast).** `m["key${i}"] = i` — no read of `m` first — lowers to `CallMethod
  r0.set(...) [reuse]` (op 8), an in-place O(1) update.

The only difference is the intervening read: `m.has`/`m[key]` make `m` non-linear, so the compile-time
reuse marker is withheld and the update copies.

**Fix — mark the self-update reuse and let the runtime guard decide.** The VM's
`map_update_in_place`/`list_set_in_place`/`set_update_in_place` **already** check `refcount == 1` and
fall back to a copy when the receiver is aliased. So it is sound to set `reuse = true` on *every*
self-update `m = m.method(…)` of a directly-held local, even when `m` was read earlier — the read
paths (`has`, index) borrow `m` without retaining it, so at the update point `refcount` is 1 and the
VM mutates in place; a genuine alias makes it > 1 and it copies, preserving value semantics. Small
blast radius (loosen the reuse-marking condition; the runtime fast path exists). Turns the RMW idiom
O(n²) → O(n): `b_wordcount_fn` ~2770 ms → an expected ~100 ms (in line with the top-level variant).

## Finding 2.5 — fused condition branch — ✅ DONE (`0aab2f0`, P-VMT-CBR)

Every `if`/`while` condition emitted `RequireCondBool` + `JumpIfFalse` (two dispatches per test).
Fused into one `Op::CondBranch` (check-bool-and-branch), byte-identical, the condition's `Binary` left
untouched so operator-overload dispatch is unaffected. b_loop 601 → 509 ms (~15%), empty 2M loop 74 →
67 ms (~10%); call-bound code flat. VM-only, differential/leak green.

## Finding 3 — loop-invariant constants reloaded every iteration — ✅ DONE (`783c7c2`, P-VMT-LICM)

**Fixed.** A bytecode pass on the monotonic pre-coalesce code hoists primitive `LoadConst`s out of
loops into a pre-header, when the register is defined once, read only by borrowing arithmetic, and not
a jump target (a merge point). b_loop's loop body drops from 3 in-loop `LoadConst`s to zero (~622 →
~435 ms); gains scale with arithmetic density (the empty loop, whose hoisted loads are cheap next to
its global accesses, is ~flat — `LoadConst` is one of the cheapest ops, so this is the smallest of the
structural wins). Differential/leak/tests green, no snapshot churn. **This was the last worthwhile
interpreter-level slice** — see the JIT plan (`../jit/README.md`).

## Finding 4 — the interpreter dispatch floor (the structural ceiling)

Even with 1–3, an empty loop iteration is ~40 ns (post-S3) vs PHP ~3.6 ns. Closing that needs one of:
**superinstructions** (fuse the hot triples — compare+jump, arith+store, the loop back-edge) to cut
dispatches per iteration; **unchecked register access** (`get_unchecked` on the frame window under the
compiler's proven-in-range invariant, removing a bounds check per operand); or ultimately a **JIT**.
The first two are interpreter-level and bench-guardable; a JIT is a milestone of its own. This is the
last-mile gap after the cheap structural wins above.

## Status & what's left (2026-07-03)

Findings **2, 1, and 2.5 are done** — the cheap, high-ROI structural wins. Cumulative vs pre-work,
and vs PHP 8.4:

| workload | before | now | PHP gap (was → now) |
|---|--:|--:|--:|
| wordcount in-fn (RMW) | ~2770 ms | ~68 ms | ~250× → **~10×** |
| loop 10M (top-level) | 1574 ms | ~509 ms | 103× → **~35×** |
| fib(32) (global calls) | 669 ms | ~490 ms | 28× → **~17×** |
| SoA column vec | 24 ms | ~22 ms | **lang wins 2.3× vs JIT** |

**The easy structural gains are now spent.** What remains splits into two tiers:

- **Incremental (interpreter-level, each ~10–20% on loops, safe, bench-guardable):** Finding 3 (LICM —
  hoist loop-invariant `LoadConst`/never-reassigned-global loads out of the loop; ~20% on b_loop but a
  real loop-analysis pass with register-lifetime interaction), and further superinstructions (fuse the
  increment `i = i+1`, or a compare-and-branch that handles the object/enum operator-overload case via
  a synchronous `call_value` fallback). Diminishing returns — each is one or two fewer ops per iter.
- **Structural (the real ceiling):** the empty-loop dispatch floor is ~33 ns/iter ≈ **3.3 ns/op** — a
  switch interpreter's floor, already near-optimal for `match`-based dispatch (LLVM compiles it to a
  jump table). PHP's ~0.3 ns/op equivalent comes from its **tracing JIT**. Closing the last ~10× on
  hot scalar/loop code needs either **threaded dispatch** (hard in stable Rust — no computed goto; the
  `become` tail-call path is unstable) or a **JIT** (a milestone of its own). No interpreter-level tweak
  crosses that gap.

**Recommendation:** LICM (Finding 3) is the last worthwhile interpreter-level slice (~20% on loops).
Past that, meaningfully closing the loop/call gap is a JIT-scale effort — worth planning as its own
milestone rather than chasing sub-10% peephole slices. The design already *wins* where it targets (SoA
column math), so the strategic question is whether general scalar-loop parity with a 25-year JIT engine
is a goal worth a JIT, or whether the effort is better spent elsewhere.

Each landed slice shipped a criterion bench (`vm_map_rmw`, `vm_dispatch/global_loop`, plus the existing
`loop_sum`/`fib`) and stayed invisible to `RunResult`, so the differential's `0 skipped / agree` gate
held by construction throughout.
