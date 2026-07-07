# JIT compile-throughput investigation (P-JCT)

**Status: ✅ ARC COMPLETE (C0–C4, branch `jit-compile-throughput`). Headline (pinned interleaved
A/B vs `28f33b2`): compile 3.1× faster (mixed fixture 1722 → 562 ms, worst pause 151 → 51 ms),
clif IR −60%, machine code −50% — and the generated code is *faster* too (small hot fns 1.35×,
mixed 1.09×, fib(30) end-to-end 1.18×, jit_promo wall 1.05×).** Spun off from P-PAR S4 (`plans/perf/parallel-seams/s4-offthread-jit.md`):
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
| C3 | IR-volume reduction: entry-block `ConstPool`, shared bail block per pc, guard-strengthened claims (results + operands + `CondBranch` Bool), dead `band` dropped | ✅ DONE |
| C4 | Pinned interleaved A/B vs `28f33b2` (runtime quality + wall) + full gates | ✅ DONE |

## C3 shipped (three stacked changes, each measured on the mixed fixture, verifier off)

| stage | clif insts | compile total |
|---|--:|--:|
| C1 exit (verifier off only) | 838,977 | 1332 ms |
| + `ConstPool` (once per body, entry block) + dead `band` + shared bail per pc | 530,792 | 928 ms |
| + result claims: asym `(Int,Imm)→Int` / `(Float,Imm)→Float` | 363,112 | 642 ms |
| + operand strengthening (guards claim the guarded side downstream; heap map clears both operands at every supported Binary; `CondBranch` claims Bool) | **334,832** | **562 ms** |

**Vs the shipped S4 config (verifier on): compile 1722 → 562 ms = 3.1×; insts −60%; machine
code 5.1 → 2.5 MB.** The 1-stmt body went 338 → 223 insts and the generic float dispatch on
provably-int adds is gone (claims now flow through the asymmetric guarded path in both
directions). Soundness rails: emitter/analysis lock-step (`def_raw` at every strengthened
guard), the typed⇒!heap invariant preserved by strengthening heap at a superset of the kind
sites, `slot_hazard_map` unaffected (hazards raised at def sites persist — a guard can't
un-track a stale slot). All suites + jit-differential green.

**Finding: the egraph is superlinear in body size.** At `opt_level=none` compile time is linear
in insts (~0.75 µs/inst at every size); at `speed` the per-inst cost grows with body size
(1.27 → 1.89 µs/inst from 40 to 160 stmts). Very large bodies would benefit from a size-tiered
opt level, but Cranelift fixes `opt_level` per ISA/module — it would need a second engine on the
compile thread.

## Post-C3 `opt=none` re-measurement (2026-07-07, pinned A/B, same binary, median of 5)

The hypothesis: with C3's static claims doing much of what the egraph cleaned up dynamically,
maybe the code-quality gap shrank enough to default to `none` (linear compiles, superlinearity
gone). **Measured — it did not.** The egraph still earns its keep on the JIT's target workload:

| workload (runtime-dominated) | compile: none faster | generated code: `speed` vs `none` |
|---|--:|--:|
| 30× 5-stmt hot fns × 2,000,000 calls | 1.80× | **speed +8.5% faster** |
| mixed fixture × 100,000 calls | 2.19× | −2.5% (a wash, within noise) |

**Verdict: keep `speed` as the production default.** On tight hot loops — exactly the code that
goes native and runs millions of times, the JIT's whole reason to exist — `speed`'s GVN/LICM buys
8.5%, which outweighs a compile-time win that S4 already moved off the mutator (it's
latency-to-native, not pauses). On larger/one-shot bodies the gap is noise. A hotness-tiered opt
choice (OSR loops → `speed`, one-shot promotions → `none`) is the only version that could win on
both axes, but it needs the two-engine complexity above for a marginal payoff — **not worth it;
the egraph is not a profitable target.** `NOETA_JIT_OPT` stays a dev knob.

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

## C4 — pinned interleaved A/B vs `28f33b2` (taskset -c 2, median of 5–7, same day)

| Measurement | base `28f33b2` | new | ratio |
|---|--:|--:|--:|
| jit_pause mixed: compile total / worst pause | 1722 ms / 151 ms | **562 ms / 51 ms** | **3.1×** |
| clif insts / code bytes (mixed, 61 bodies) | 839 k / 5.08 MB | 335 k / 2.54 MB | −60% / −50% |
| generated-code wall, 30×5-stmt fns × 2 M calls | 6163 ms | **4570 ms** | **1.35×** |
| generated-code wall, mixed × 100 k calls | 18110 ms | 16543 ms | 1.09× |
| fib(30) end-to-end (`noeta run`, compile lands mid-flight) | 23.9 ms | 20.2 ms | 1.18× |
| jit_promo.noe end-to-end (`noeta run`) | 36.9 ms | 35.0 ms | 1.05× |

The code-quality wins come from the strengthened claims (generic dispatches and repeat guards
gone from hot loops); the end-to-end wins from compiles landing ~3× sooner (S4's off-thread
service drains its backlog faster, so tier-1 entry happens earlier).

**Measurement trap (recorded in the example too): `jit_pause`'s `wall − compile` is NOT runtime
under off-thread compilation** — compile overlaps the mutator and the stats entry drains the
queue at exit, so a compile-bound run reports ~0. Compare `wall` at runtime-dominated call
counts for code quality.

Final gates: 501 conformance, differential + jit-differential (tier 1 agrees, leaks nothing),
`cargo test --workspace` all suites, clippy + fmt — green.
