# Slice M1.0 — Value spine + minimal VM + differential oracle

Status: todo

## Goal
Stand up the NaN-boxed value representation, a register bytecode VM, and the differential oracle — compiling the smallest AST subset (literals, bindings, arithmetic, `~`) end-to-end and asserting `VmBackend` ≡ `TreeWalkBackend` on every corpus case it can compile.

## Scope
- In:
  - **`lang-backend`** crate: move `trait Backend` + `RunResult` out of `lang-eval` (mechanical), so `lang-eval` and `lang-vm` are siblings (no `vm → eval` edge). Update `eval`/`conformance`/`cli` imports.
  - **`lang-value`** crate: `struct Value(u64)` NaN-box codec (immediates: unit, bool, int, native double; boxed: heap pointer in low 48 bits with a type tag); the heap-object header (`{ refcount: u32 non-atomic, tag/shape word, len }`); safe encode/decode accessors; the boxed-vs-packed **classification** enum + `TypeId` newtype (type-system vocabulary, reserved for the checker). `unsafe` allowed locally. Headline test: **proptest** `decode(encode(v)) == v`.
  - **`lang-gc`** crate (skeleton): manual `inc_ref`/`dec_ref`/free for the one heap type this slice needs (string). No cycle collector yet.
  - **`lang-bytecode`** crate: opcode enum (`LOAD_CONST`, `MOVE`, `ADD`/`SUB`/`MUL`/`DIV`/`MOD`, `CMP_*`, `NOT`, `NEG`, `CONCAT`, `ECHO`, `HALT`), `Chunk` (register count, constant pool, line table), and a **disassembler** to stable text.
  - **`lang-compiler`** crate: AST → `Chunk` for the subset; trivial register allocator (temporary stack); hard `Unsupported(node)` error for anything else.
  - **`lang-vm`** crate: Tier-0 register dispatch loop; `VmBackend: Backend`. `unsafe` allowed locally.
  - **`lang-conformance`**: `--differential` mode — run each program through both backends, skip (not fail) cases the VM reports `Unsupported`, assert identical `RunResult` (stdout, exit, diagnostic code+span) for the rest; print climbing coverage %.
  - Workspace: add `salsa`, `gc-arena`, `criterion` to `[workspace.dependencies]` (used from later slices; pin now). Reserve `benches/`.
- Out: functions/closures (M1.2), collections (M1.3), objects/shapes (M1.4), the salsa query layer (M1.1), any type checking.

## Checklist (vertical slice)
- [ ] Grammar / AST: none (reuses the existing M0 AST unchanged).
- [ ] Checker rule: n/a (deferred to M1.7; classification vocabulary reserved in `lang-value`).
- [ ] Bytecode: opcode set + `Chunk` + disassembler (`lang-bytecode`); AST→`Chunk` lowering for the subset (`lang-compiler`).
- [ ] VM op: register dispatch for every opcode above; `VmBackend: Backend` (`lang-vm`); `lang-value` codec; `lang-gc` string refcount.
- [ ] Conformance cases: `--differential` covers the existing arithmetic/binding/`~` cases; add `vm/value_roundtrip` notes if needed. No new `.lang` features, so the existing corpus subset is the spec.
- [ ] Snapshots: disassembly snapshots for representative chunks (`lang-bytecode` or `lang-vm` tests); proptest equal-`RunResult` over the supported AST subset.

## Definition of done
- `cargo run -p lang-cli -- test --differential` runs, reports the % of corpus the VM compiles, and shows **zero divergence** on compilable cases.
- `lang-value` proptest round-trip green; `miri` green on `lang-value`/`lang-vm`/`lang-gc` test suites.
- `lang-backend` extraction leaves `cargo test --workspace` green (tree-walker behavior unchanged).
- fmt/clippy clean; `unsafe` appears only in `lang-value`/`lang-gc`/`lang-vm`, each overriding the workspace `forbid` locally.

## Notes / traps
- **Do not share `Value` between backends.** The M0 `Value` enum (Rc-based) stays in `lang-eval` as the oracle; comparison is `RunResult`-only. An `Rc<T>` cannot live in a NaN-box pointer slot — the memory models must stay separate.
- This slice defines the layout everything else inherits; the NaN-box encoding, heap header, and opcode format are the hardest things to change later. Get the codec + header right before adding features.
- Keep the `unsafe` surface small and pure (codec functions, not whole subsystems) so miri can actually cover it.
