# Phase 2 — VM lowers from Core IR

Re-point the compiler so the VM lowers **Core IR → bytecode** instead of AST → bytecode. After this
phase *both* backends execute the same lowered IR — the shared-foundation milestone — while
reclamation behavior is still exactly today's. This is pure plumbing/faithfulness; no RC change, no
observable change.

## 2.1 Re-target the compiler

- `lang-compiler` consumes `lang_ir::CoreIr` (from Phase 1's lowering) and emits `lang-bytecode`. Most
  of the existing `FnCompiler::expr/stmt` logic survives but is *simplified*: ANF means operands are
  already atoms, so the compiler no longer recursively flattens nested expressions into temp registers
  — that flattening is now the IR's `let`-sequence. Register allocation is still monotonic this phase
  (the reuse-aware allocator is Phase 3).
- The compiler becomes a structural 1:1 lowering of IR ops → bytecode ops (a `let v = a + b` →
  `Op::Binary`, a constructor → `Op::MakeList`/`MakeRecord`/…, an IR `if` → branch ops, etc.). Far less
  semantic logic than AST→bytecode, because evaluation order and naming are fixed by the IR.
- The empty RC-annotation slots (Phase 1) lower to nothing yet.

## 2.2 What this buys (and costs)

- **Both backends now run one program.** The IR-interpreter (Phase 1) and the IR-lowered VM consume the
  *identical* `CoreIr`. Cross-backend agreement on *meaning* is now structural; the differential's job
  narrows to validating the VM's bytecode lowering + manual-RC mechanism against the IR-interpreter's
  Rust-`Rc` mechanism (README §2 — the MM-relevant independence we deliberately keep).
- **Cost paid here:** any lowering bug now affects both backends. Mitigations: the Phase-1 faithfulness
  differential (old AST-walker still runs, catching IR-semantics regressions independently); IR golden
  tests; and the still-live eval-vs-VM differential catches VM-lowering/codegen bugs.

## 2.3 Subset & skip handling

The IR carries `Unsupported` markers for nodes outside the VM subset (Phase 1); the compiler propagates
them so the differential still *skips* (never silently miscompiles) the same programs it skips today.
Target: **0 newly-skipped** programs — the IR lowering must cover everything the AST→bytecode path did.

## Verification gate

- Conformance + **differential 0 skipped / agree** (now: IR-interpreter vs IR-lowered-VM), *and* the
  Phase-1 faithfulness differential (old-eval vs IR-eval) still green — two independent checks bracket
  the IR.
- Leak oracle unchanged (reclamation behavior identical to today).
- Bench: dispatch/property/allocation within noise — the IR lowering must not regress the hot path
  (ANF can add register pressure; the Phase-3 reuse-aware allocator reclaims it, but P2 must not regress
  materially — record and watch).
- miri unchanged surface; clippy + fmt clean.
