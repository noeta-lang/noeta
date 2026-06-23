# Slice S7 — Migration finalize + cleanup (closing slice)

Status: **done**. Conformance 169 / differential 163 matched / 0 skipped / backends agree. Branch `types-inferred-static`. This is the **closing** slice of the inferred-static type-system track — it runs after every feature slice (S0–S6, S8, plus the mid-track L1–L4, optional parameters, and `while`/ranges/`break`/`continue`) has landed.

> **Track:** inferred-static type system (see `plans/types/README.md`). **Runs last** by design: finalize the migration the feature slices performed and clear the in-progress scaffolding they left behind.

## What "finalize" meant in practice

The corpus migration — flipping example/conformance expectations from *run* to *reject* as the static layer caught more — was performed **incrementally, inside each feature slice** (S2 required-signatures, S3b argument checking, S4 bounds, S5 coherence, S6 narrowing, S8 unions). By the time S7 opened there was nothing left to migrate: the examples (`examples/hello.lang`, `examples/orders.lang`) run green, the full conformance suite passes, and the differential oracle is at 0 skipped with both backends agreeing. So S7 carried **no behavior change** — it is a pure finalize-and-cleanup pass that leaves the verdict of every program identical.

## What landed (cleanup)

1. **Removed dead `Type::Var`.** The lattice carried a `Var(u32)` inference-variable variant "reserved for the unification front that hardens inference." That front is the thing the track's **locked decision explicitly rejected** — the engine is bidirectional checking with local inference, *not* Hindley–Milner unification — so `Var` was constructed nowhere outside its own unit tests across the entire S0–S8 build. It is now removed: the variant, its `Display` arm (`?{n}`), its `is_gradual`/`defers_to_runtime` membership, and the two tests that referenced it. Local inference uses `Type::Unknown` holes, not numbered variables; if numbered vars are ever wanted, re-adding a variant is trivial.

2. **Reframed the "gradual tolerance" prose from in-progress to settled.** The `lang-types` and `lang-check` module docs (and several per-item doc comments) described gradual tolerance as the *current* state and promised "later slices remove the hole fallback" / "the strict flip later" / "in this slice every caller passes `Unknown`." Those forward-references are now resolved, and one was outright false post-S3b/S3c (production callers *do* pass real expectations — declared returns, argument types, declared element types — so `Checker::check`'s propagation arms fire and `subsume` enforces `actual <: expected`). The prose now states the **settled** posture accurately:
   - Holes are eliminated at **every typed boundary** — required signatures (`E0022`), checked arguments and returns, and the `E0023` binding endpoint.
   - A residual tolerance remains **by design** for an *interior* hole (an un-typed prelude result, a numeric hole) where flagging would risk a false positive. This is a recorded design choice, not pending removal — it is `Type::is_gradual` / `defers_to_runtime`, and it is what keeps the shared checker from diverging the differential oracle.

3. **Refreshed `crates/lang-check/README.md`** — the M1.9-era line claiming inference is "a conservative name-first gradual pass, not yet full Hindley–Milner … the lattice and `Type::Var` are in place for that hardening" contradicted both the removed `Var` and the locked not-HM decision. It now describes the bidirectional engine and what the type track built.

## Deliberately left alone

- **"Gradual" as a descriptive adjective for a hole** (e.g. "the operand is often gradual", "a numeric hole — gradual, not the `dyn` escape") stays — it accurately names the *interior-hole* condition that is tolerated by design.
- **Forward-references in *other* tracks' code** (`lang-value`/heap field-assignment, `lang-compiler` self-capture, `lang-bytecode`/`lang-vm` builtins-as-values) — these point at genuinely future non-type-track work and remain accurate; out of S7's scope.

## Track status after S7

The inferred-static type-system track is **complete**: S0 lattice/`dyn` → S1 bidirectional engine → S2 signatures (E0022) → S3a/S3b stdlib + argument checking → S3c inference completion (E0023) + L1–L4 list-building → S4 bounded generics (E0025) → S5 trait coherence (E0027) → S6 `dyn` narrowing (E0028) → S8 declared unions → **S7 finalize**. Diagnostic codes E0022–E0028 (plus the mid-track E0024 loop-control, E0026 required-after-optional); next free is **E0029**.

Deferred beyond the track (recorded, not lost): ternary / conditional-expression in an `if let`-style surface; exhaustive `match` over a union (needs type-patterns); structural intersection `A & B` (the useful form is S4 trait bounds); `TypeId` interning (throughput, awaits a benchmark). Next up the broader sequence: the attribute-system pass, then the perf deferred items.

## Verification
- `cargo run -q -p lang-cli -- test` → conformance green.
- `cargo run -q -p lang-cli -- test --differential` → matched / 0 skipped / backends agree.
- `cargo test --workspace` → green.
- `cargo clippy --all-targets` + `cargo fmt --all --check` → clean.
