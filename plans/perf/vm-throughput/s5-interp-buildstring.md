# S5 — Single-pass string interpolation (P-VMT-STR)

**Status: DONE.** `lower_interp` now emits a single `Op::BuildString { dst, parts: Box<[StrPart]> }`
instead of the `LoadConst "" + N×(Stringify + Concat)` left-fold. Each `StrPart` is a constant-pool
`Literal` or a `Hole` register; the VM sizes one buffer from the literal lengths, then pushes each
literal verbatim and renders each hole via `display` — no intermediate `String`s, one output
allocation. A `Display`-object hole still dispatches to `to_string` through the per-hole `Stringify`
that precedes the build (that op pushes a call frame; `BuildString` itself never does), so semantics
are byte-identical to the fold. The tree-walker was already single-pass, so this only brings the VM
to parity — eval is untouched.

Result (release, before = the S3 nested-loop commit, same session):

| bench | before (fold) | after (`BuildString`) | speedup |
|--|--:|--:|--:|
| `single_hole` `"word${i}"` 1,000,000 | 303.6 ms | 149.7 ms | **2.03×** |
| `single_hole` 100,000 | 29.3 ms | 14.2 ms | **2.06×** |
| `multi_hole` `"${i}-${i}-${i}"` 100,000 | 58.6 ms | 21.4 ms | **2.74×** |
| `multi_hole` 1,000,000 | ≈586 ms (extrap.) | 214.0 ms | **~2.7×** |

The multi-hole case gains more because the old fold's cost grew O(k²) in the number of parts (each
intermediate concat reallocated and recopied the accumulator); `BuildString` is O(total length) with
one allocation regardless of `k`.

Behaviour-neutral (output byte-identical to the fold): differential 419 / 0 skipped / backends agree,
corpus 430, leak oracle residency 0 on both backends, workspace tests green, clippy clean. Criterion
bench `vm_interp/{single,multi}_hole/{100000,1000000}` added.

**Original goal.** Make `"…${x}…"` cost one pass and one allocation, not an N-way concat fold. String
interpolation is the dominant cost in the worst benchmark outlier (wordcount, ~350× behind PHP).

## Evidence

Ablation: adding `key = "word${i % 500}"` to the loop costs **+262 ns/iter** — more than the entire
rest of wordcount. `lower_interp` (`crates/lang-compiler/src/lib.rs` ~2612) lowers
`"word${i%500}"` to:

```
dst = ""                       // LoadConst empty
r = "word";  dst = dst ~ r     // Binary Concat  (allocates: "" ~ "word")
r = Stringify(i%500); dst = dst ~ r   // Stringify (alloc) + Binary Concat (alloc)
```

So a 1-hole interpolation is ~3–4 heap allocations (empty seed, each concat builds a new `String`,
plus the `Stringify`), and it grows linearly with parts — each intermediate concat reallocates and
copies everything so far (a small O(k²) in the number of parts).

## Approach

Add a single build-string instruction and lower interpolation to it:

1. New `Op::BuildString { dst, parts: Box<[StrPart]> }` in `lang-bytecode`, where `StrPart` is either
   a constant-string id or a register (to be `display`-stringified). One op, one output `String`.
2. VM/eval implement it by computing the total length where cheap, `String::with_capacity`, then
   pushing each literal verbatim and each hole via the existing `display`/`to_string` path (routing a
   `Display` object through its `to_string`, exactly as `Op::Stringify` does today) — **no
   intermediate `String`s**.
3. `lower_interp` emits one `BuildString` instead of the `LoadConst "" + N×(Stringify + Concat)` fold.
   Semantics are identical to the current left-fold (same `display` per part, same order), so it stays
   observationally equal.

## Files

- `crates/lang-bytecode/src/lib.rs` — `Op::BuildString` + `StrPart`; disassembler.
- `crates/lang-compiler/src/lib.rs` — `lower_interp` emits the new op.
- `crates/lang-vm/src/lib.rs`, `crates/lang-eval/src/lib.rs` — execute `BuildString` (shared `display`
  logic so both agree).

## Validation

- **Benchmark:** criterion `vm.rs` — an interpolation-heavy loop (`"word${i % 500}"` ×N) and a
  multi-hole case (`"${a}-${b}-${c}"`). Target: collapse the +262 ns/iter toward one alloc's worth.
  Re-run the wordcount `scratch-bench` pair as the end-to-end check.
- **Oracle:** the output string is byte-identical to the fold → invisible to `RunResult`, differential
  `0 skipped / agree`. Existing interpolation conformance is the correctness net (many cases already);
  add a multi-hole + `Display`-object-in-hole case if thin.

## Risk

Low–medium. The one thing to preserve exactly: the per-hole `display` semantics (whole-float `2.0`,
`some(x)`/`Ok(x)` forms, `Display` objects via `to_string`) — reuse the current `Stringify` path
verbatim so the rendered text can't drift between the fold and the single-pass builder.

## Dependencies

None (independent of S1–S4). Lands anytime after S0; grouped into the arc here because wordcount is
the worst outlier and this is the slice that moves it.
