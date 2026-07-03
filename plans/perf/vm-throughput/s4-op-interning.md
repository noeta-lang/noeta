# S4 — Shrink `Op` via name interning (P-VMT-OPSZ)

**Status: DONE** (two sub-commits). `size_of::<Op>()` went from **128 → 48 bytes** — from two cache
lines per instruction to comfortably within one. Split into **S4.1 (interning, `fcc055a`)** and **S4.2
(boxing, `d47b4d6`)**, the last slice of the arc.

- **S4.1 — name interning (128 → 88 B).** Every instruction-embedded name (field / method / global /
  type names, the ext-call module+func, and `match`-literal strings) was an inline 24-byte `String`;
  the widest such variant (`ExtCall`, two Strings) pinned `Op` at 128. Each becomes a 4-byte `NameId`
  indexing a new module-wide `Module::names` table, deduped by the compiler's `intern_name`. The VM
  resolves a `NameId → &str` (via `Module::name`) only at the cold lookup sites (global / method /
  field resolution, which then hit a hashmap or field scan anyway), so the hot path is untouched. The
  disassembler resolves ids too, so its output is byte-identical to the inline-`String` form.
- **S4.2 — box the wide cold payloads (88 → 48 B).** After interning, `Op` was still 88 B, pinned by
  three cold variants carrying a wide payload inline: `ExtCall`'s `Option<TypeRecipe>` (48 B),
  `TypeOfStatic`'s `TypeRepr` (56 B), and `Narrow`/`IsType`'s `NarrowTarget` (32 B). Box each — all
  are cold ops (a call-site-typed native call, `type_of`, narrowing), so the added indirection never
  touches the hot loop. `Box<T>` renders transparently in `Debug`, so the disassembly is unchanged.

**Structural result (exact, the primary win):** `Op` = **48 B**, down from 128 — a 2.7× shrink of the
instruction stream, and each instruction now fits in a single 64-byte cache line instead of straddling
two. A new `crates/lang-bytecode/tests/op_size.rs` asserts the one-cache-line bound so a future variant
can't silently regress it.

**Runtime result (modest, near the machine's noise floor).** The icache win is broad and small, as the
plan anticipated. On dispatch-bound loops the before/after medians move consistently in the right
direction; call-heavy microbenchmarks (`member_dispatch`, `dispatch_fib`) sat within this machine's
run-to-run variance and are not claimed.

| bench | before (S4.1 parent) | after (S4.2) | Δ |
|--|--:|--:|--:|
| `vm_dispatch/loop_sum` 1,000,000 | 46.7 ms | 43.6 ms | **−6.7%** |
| `vm_dispatch/loop_sum` 100,000 | 4.10 ms | 3.88 ms | **−5.3%** |
| `vm_recursion/fib` 24 | 16.2 ms | 15.0 ms | **−7.2%** |
| `vm_recursion/fib` 28 | 103.2 ms | 98.9 ms | **−4.2%** |

Representation only — invisible to `RunResult`: differential 419 matched / 0 skipped / backends agree,
leak-oracle residency 0 on both backends, full workspace tests green.

---

**Original goal.** `size_of::<Op>()` = **128 bytes** — two cache lines per instruction. The bytecode
stream is huge and cache-hostile; shrinking `Op` speeds up the whole dispatch loop (fewer bytes fetched
per instruction, better icache/prefetch), compounding with S3.

## Evidence

`Op` (84 variants) is sized to its widest variants, which carry **inline `String`s** (24 B each) and
`Span`s:

```
~86B  ExtCall   { dst, module: String, func: String, args, span }
~73B  EnumFromStr { dst, arg, enum_name: String, … }
~69B  CallMethod  { dst, recv, method: String, args, span, cache, reuse }
~58B  MakeStruct / MakeOpaque { … type_name: String, keys: Box<[(String,Reg)]> … }
~44B  LoadField   { dst, obj, field: String, span }
       LoadGlobal / StoreGlobal / TakeGlobal { name: String, … }
```

## Approach

Intern all instruction-embedded names to `u32` IDs held in a per-`Module` string table:

1. Add a `names: Vec<String>` (or interner) to `Module`/`Chunk`; every `String` field in `Op`
   (`method`, `field`, `name`, `module`, `func`, `type_name`, `enum_name`, keys) becomes a `NameId(u32)`.
2. The **compiler** interns at emit time (it already builds the `Module`); the **VM** resolves
   `NameId → &str` only where needed (global lookup, method-table key, field-name scan) — and the
   inline-cache (P-IC) already short-circuits the hot method/field cases, so most resolutions are cold.
3. Box the few remaining wide payloads that aren't just names — `MakeStruct`/`MakeOpaque`'s
   `Box<[(NameId, Reg)]>` is already boxed; ensure no variant carries a large inline array.
4. ~~Shrink/relocate `Span`: intern spans into a side table indexed by pc to reach `Op` ≤ 32 B.~~
   **Not done — deliberate.** Steps 1–3 landed `Op` at **48 B, already within one 64-byte cache
   line**; the 128 → ≤64 crossing is where the icache win lives (an instruction stops straddling two
   lines). The remaining 48 → ~32 would require pulling the 12-byte `Span` off ~40 op variants into a
   pc-indexed side table and rerouting every VM error path through it — a large, invasive change to the
   *cold* error paths (which the differential covers less densely than the happy path) for a
   sub-cache-line gain that crosses no new boundary. Per "build it right, not easy," the sound call here
   is to stop at the cache-line boundary, not to chase a round number. Left as a documented option if a
   future profile shows the instruction stream is still bandwidth-bound.

Update the **disassembler** (`lang-bytecode` `Display`/pretty) to resolve `NameId`s so `--dump`/tests
stay readable.

## Files

- `crates/lang-bytecode/src/lib.rs` — `Op` variants (`String`→`NameId`), `Module`/`Chunk` name table,
  disassembler.
- `crates/lang-compiler/src/lib.rs` — intern names at emit; any bytecode-shape assertions/tests.
- `crates/lang-vm/src/lib.rs` — resolve `NameId` at the (cold) lookup sites; method/field/global paths.

## Validation

- **Benchmark:** criterion `vm.rs` — the same dispatch + call-method benches; the win is broad and
  modest (icache), so measure across several. Record `size_of::<Op>()` before/after in the slice doc
  (128 B → target ≤ 32 B) as a structural check.
- **Oracle:** representation-only, invisible to `RunResult` → differential `0 skipped / agree`.
  Bytecode disassembly tests updated to the interned form (readability preserved).

## Risk

Medium–large — the **cross-crate blast radius** (bytecode + compiler + VM + disassembler + any test
that pattern-matches `Op` variants). Mechanical but wide; land last so it rebases onto S1–S3/S5 rather
than the reverse. No behaviour risk (names still resolve to the same strings).

## Dependencies

Independent of S0–S2; **co-schedule with S3** (both touch the hot loop — do S3's structural rewrite
first, then S4 shrinks the instruction it streams). Sequenced **last** in the arc for blast-radius
reasons.
