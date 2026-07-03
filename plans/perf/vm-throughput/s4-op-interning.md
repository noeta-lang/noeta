# S4 — Shrink `Op` via name interning (P-VMT-OPSZ)

**Goal.** `size_of::<Op>()` = **128 bytes** — two cache lines per instruction. The bytecode stream is
huge and cache-hostile; shrinking `Op` speeds up the whole dispatch loop (fewer bytes fetched per
instruction, better icache/prefetch), compounding with S3.

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
4. Shrink/relocate `Span`: either intern spans into a side table indexed by pc, or keep a compact
   `Span` (it's only read on the error path, never on the hot path — a side table keeps it off the
   instruction entirely). Target: `Op` ≤ 32 B.

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
