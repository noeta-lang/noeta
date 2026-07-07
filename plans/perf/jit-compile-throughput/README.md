# JIT compile-throughput investigation (P-JCT)

**Status: IN PROGRESS.** Spun off from P-PAR S4 (`plans/perf/parallel-seams/s4-offthread-jit.md`):
even at `NOETA_JIT_OPT=none` the engine averages **~27 ms per compile** for functions a few lines
long — Cranelift's own ballpark for small functions is tens–hundreds of **µs**, so we are paying
2–3 orders of magnitude somewhere in *our* embedding, not in the optimizer (the opt-level A/B
already ruled the egraph out as the dominant term). Off-thread compilation (S4) hid the pauses;
this arc attacks the cost itself — cheaper compiles mean hot code goes native sooner (fib(30)'s
−4.6 ms concurrency tax shrinks), less compile-thread backlog, and less energy.

## Suspects (ranked, with priors)

1. **`enable_verifier` — defaults ON.** Confirmed in cranelift-codegen 0.133.1
   (`settings.rs:502`): `Jit::new` sets `opt_level`/`is_pic`/`use_colocated_libcalls` but never
   touches the verifier, so every compile runs full IR verification (multiple passes at
   `opt_level=speed`). Production embedders (wasmtime) disable it outside debug. Semantics-free:
   verification only *checks* IR, it never changes codegen.
2. **Per-body `finalize_definitions()`** (`lib.rs finalize()`): each of the ~2 bodies per proto
   pays relocation + memory-protection flips (W^X page transitions) for the whole module.
3. **Two bodies per proto** (classic + fast convention): a fixed 2× on eligible protos.
4. Residual: regalloc on long straight-line chains (jit_promo's 100-stmt bodies), IR volume per
   bytecode op (~235 `b.ins()` sites; call-heavy ops emit big sequences).

## Slices

| # | Slice | Status |
|---|---|---|
| C0 | Instrument: per-phase ns (IR build / define / finalize), clif inst + code-byte counts, surfaced in `jit_pause`; `NOETA_JIT_CLIF` IR dump | ✅ DONE |
| C1 | Verifier A/B → policy (off in release; on under `debug_assertions`; `NOETA_JIT_VERIFY=1|0` overrides) — **−23% compile time** | ✅ DONE |
| C2 | Finalize batching | ✅ CLOSED, measured dead: `finalize_definitions` = 0.6 ms of a 1740 ms compile total (~10 µs/body) — nothing to batch |
| C3 | IR-volume reduction (the real story — see findings): claims propagation, iconst dedup, shared bail blocks, dead `band` | pending |
| C4 | Pinned interleaved A/B re-run of the S4 benches + full gates | pending |

## C0 findings — the cost is IR volume, not Cranelift

Baseline (mixed 5/40/160-stmt fixture, opt=speed, verifier on — the shipped S4 config):
**define_function is 97% of compile time** (1683 of 1740 ms); IR build 56 ms; finalize 0.6 ms.
Volume: **838,977 clif insts / 61 bodies ≈ 13,754 insts per body ≈ 200 insts per source
statement** (~50 per bytecode op), 5.1 MB machine code (~83 KB/body). Throughput 3–5 MB/s —
Cranelift's normal range — so Cranelift is fine; **we hand it 10–20× more IR than the program
warrants**. Scaling is ~linear in statements (5→40→160 stmts: 1139/8110/32012 insts per body;
marginal ~200/stmt constant; fixed overhead 139 insts/body). Where the ~200/stmt goes
(`NOETA_JIT_CLIF` dump of a 1-stmt fn):

1. **Weak kind claims** — `a*3 + x%2 + 1`'s outer `+` emits the full generic dispatch (two-sided
   int check, complete float fast path with NaN canonicalization, ~50 insts) even though both
   operands were *just produced as ints* by the preceding ops. Claims don't propagate through
   int-result defs or past guards. Fixing this also makes the *generated code* faster.
2. **iconst spam** — the same constants (16, `0xffff_ffff_ffff`, tag words) re-emitted 4–6× per
   op; roughly a third of all instructions are `iconst`. GVN cleans it up downstream, but we pay
   compile time proportional to input insts.
3. **Per-guard bail blocks** — ~5 guards/stmt, each with its own bail block doing a full frame
   sync (`sync_frame`); guards of the same pc could share one.
4. Micro: the `band ptr_mask` before `ishl 16; sshr 16` untag/fit sequences is redundant (the
   shift pair already discards the top 16 bits).

## C1 numbers (jit_pause mixed, pinned, opt=speed)

| | verifier ON (old) | verifier OFF (new release default) |
|---|--:|--:|
| compile total | 1722 ms | **1332 ms (−23%)** |
| compile max (worst body) | 151 ms | 121 ms |
| define throughput | 3.05 MB/s | 3.99 MB/s |

Policy shipped in `Jit::new`: `enable_verifier` = `cfg!(debug_assertions)` unless
`NOETA_JIT_VERIFY` overrides — debug builds (test suites, oracle CI) keep the net; release stops
paying for a pure debug check. jit-differential re-run green with `NOETA_JIT_VERIFY=0` forced.

## Posture (inherited)

- Bench-first; every claim ships before/after numbers. **Pinned interleaved A/B only**
  (`taskset -c 2`, binaries from both commits) — unpinned criterion on this laptop is ±10% noise.
- Real-path only; sandbox/differential untouched by construction. jit-differential must stay
  byte-identical (verifier and finalize batching change no semantics; assert it anyway).
- Gates per slice: conformance, differential, jit-differential, workspace tests, clippy, fmt.
- Commit per green slice; never push.

## Numbers

(recorded as slices land)

| Measurement | before | after |
|---|--:|--:|
| jit_pause mixed: compile total / max | 1695.6 ms / 165 ms (S4 baseline, opt=speed) | |
| jit_promo.noe end-to-end wall | 46.9 ms (off-thread) | |
| fib(30) end-to-end (compile lands mid-flight) | 31.2 ms | |
