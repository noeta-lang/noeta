# Slice M1.0 — Value spine + minimal VM + differential oracle

Status: done

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

## Outcome
Seven new crates landed (`lang-backend`, `lang-value`, `lang-gc`, `lang-bytecode`, `lang-compiler`, `lang-vm`, plus the `--differential` mode in `lang-conformance`). The `Backend`/`RunResult` seam moved out of `lang-eval` into `lang-backend`; the tree-walker re-exports it, so existing `lang_eval::{Backend, RunResult}` users are unchanged.

- **NaN-box scheme:** doubles native (NaN canonicalized to `0x7ff8…` so it never collides with the tag space), `QNAN`-prefixed immediates for unit/bool and a 48-bit small-int, `SIGN|QNAN|addr48` heap pointers. i64 outside the immediate range **boxes** so full i64 wrapping survives (the `9223372036854775807 + 1` corpus case passes). The pointer round-trip uses the exposed-provenance API (`expose_provenance`/`with_exposed_provenance_mut`) — the miri-sound way to stash a pointer in a word.
- **`unsafe` is quarantined to `lang-value`'s `heap` module** (alloc/free/refcount/deref); `lang-gc`/`lang-vm`/`lang-compiler`/`lang-bytecode`/`lang-backend` stay under the workspace `forbid`. The VM is unsafe-free — it manipulates values only through `lang-value`'s safe API.
- **Refcount discipline:** each register owns one reference; overwriting releases the old occupant, `Move` retains the source, and exit (normal or error) releases every register. **miri is clean** (`-Zmiri-permissive-provenance -Zmiri-disable-isolation`) over `lang-value`/`lang-gc`/`lang-vm` — no leaks, no UB. (Installed a current `nightly` + `miri` to verify; the old `nightly-2024-10-31` predates edition 2024.)
- **Differential oracle:** `cargo run -p lang-cli -- test --differential` → **10 matched, 22 skipped, 4 parse-failed; 31.2% coverage; zero divergence.** Guarded by `cargo test` (`differential_backends_agree`).
- Disassembly snapshot committed; the snapshot test is skipped under miri (insta uses `socketpair`, unsupported there) but it touches no `unsafe`.

Gates green: `cargo test --workspace`, `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`.

Deviation from the slice plan: `salsa`/`gc-arena`/`criterion` were **not** pre-pinned (they'd be unused until M1.1/M1.6); they'll be added in the slice that first uses them. The salsa db plumbing is M1.1 as planned.
