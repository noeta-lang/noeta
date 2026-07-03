# Where the PHP gap still lives — post-P-VMT investigation (2026-07-03)

After the P-VMT arc (S0–S5) closed part of the scalar/loop/call gap (loop 10M −21%, fib −9%), this
is a disassembly-driven look at *what still dominates* the remaining 5–250× gap to PHP 8.4, and the
next slices that would close it. Evidence is `lang dump <file>` on the `scratch-bench/` programs.

## Finding 1 — top-level bindings & global functions go through a string-keyed `HashMap` (highest leverage)

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

## Finding 3 — loop-invariant constants reloaded every iteration (minor)

`b_loop` re-`LoadConst`s `10000000`, `7`, `1` each iteration; `b_wordcount` reloads `500`/`"word"`.
A small loop-invariant-code-motion pass (hoist constant loads out of the loop header) would trim a few
ops per iteration. Low priority next to findings 1–2.

## Finding 4 — the interpreter dispatch floor (the structural ceiling)

Even with 1–3, an empty loop iteration is ~40 ns (post-S3) vs PHP ~3.6 ns. Closing that needs one of:
**superinstructions** (fuse the hot triples — compare+jump, arith+store, the loop back-edge) to cut
dispatches per iteration; **unchecked register access** (`get_unchecked` on the frame window under the
compiler's proven-in-range invariant, removing a bounds check per operand); or ultimately a **JIT**.
The first two are interpreter-level and bench-guardable; a JIT is a milestone of its own. This is the
last-mile gap after the cheap structural wins above.

## Suggested sequencing

1. **Finding 2 first** — smallest change, kills the worst outlier (250× → ~10×), and the runtime
   fast path already exists. Immediate, dramatic, low risk.
2. **Finding 1 next** — the broadest win (every top-level loop and global call); moderate but
   mechanical, and `lang dump` makes the before/after obvious.
3. **Finding 3** opportunistically alongside 1 (both touch loop codegen).
4. **Finding 4** as a follow-on arc (superinstructions + unchecked registers) once 1–3 land, with a
   JIT as the eventual ceiling-breaker.

Each ships with a criterion bench (findings 1–2 map directly onto `vm_dispatch/loop_sum`,
`vm_recursion/fib`, and a new wordcount-style RMW bench) and stays invisible to `RunResult`, so the
differential's `0 skipped / agree` gate holds by construction.
