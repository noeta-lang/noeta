# Slice M1.5 — Result/Option/`?`/`??` + match

Status: done

## Goal
Compile `match`, `?` propagation, and `??` coalescing to bytecode, completing the language-feature surface the VM must reproduce.

## Scope
- In: `match` compilation to jump tables / decision trees over shapes (variant dispatch + binding destructuring + literal patterns + wildcard); `Try` (`?`) as early-return via frame unwinding in the VM (on `Err`/`none`); `Coalesce` (`??`) fallback on `Err`/`none`; the `Result`/`Option` constructors (`Ok`/`Err`/`some`/`none`) and `panic` reusing the M1.4 enum representation; runtime non-exhaustive-match error.
- Out: **static** exhaustiveness checking (M1.7 promotes the runtime error to a compile error); typed `?` (M1.7).

## Checklist (vertical slice)
- [x] Grammar / AST: none (reuses M0 `Match`/`Try`/`Coalesce`).
- [x] Checker rule: n/a (M1.7).
- [x] Bytecode: match decision-tree opcodes (`MatchInt`/`MatchStr`/`MatchBool`/`MatchVariant`/`ExtractField`/`MatchFail`), `?`-unwind (`TryUnwrap`), `??`-fallback (`Coalesce`), constructor lowering (`Ok`/`Err`/`some`/`none` → `MakeEnum`, `panic` → `Panic`, `next_id` → `NextId`).
- [x] VM op: pattern dispatch, frame unwind on `?`, panic → E0010, deterministic `next_id` counter (`lang-vm`).
- [x] Conformance cases: `enums/*`, `results/*`, `classes/construct_and_method`, and the §14 `demo/orders` run on `VmBackend`.
- [x] Snapshots: disassembly of a `match` decision tree and a `?`-propagating function.

## Definition of done
- [x] **Thrust A gate:** `cargo run -p lang-cli -- test --differential` shows **100% corpus coverage** (32 matched, 0 skipped) with zero backend divergence — including the full §14 `examples/orders.lang` demo. The tree-walker is now frozen as the pure oracle.
- [x] miri green; fmt/clippy clean.

## Notes / traps
- `?` early-return is the M0 `Unwind::Return` mechanism translated to VM frame unwinding — keep the call-boundary catch semantics identical.
- After this slice the tree-walker stops being the forcing function and becomes CI insurance; any new post-Thrust-A feature must land in both backends in the same slice (or be explicitly oracle-exempt).

## Outcome

**Thrust A is complete.** The VM compiles **100% of the comparable corpus** (32/32 parse-clean cases) with **zero divergence** from the tree-walker — including the entire §14 `examples/orders.lang` demo. The conformance floor was changed from a climbing count to an exact `skipped == 0` gate that must never regress.

**Match (`lang-compiler` + `lang-vm`).** `match` lowers to a linear decision chain: each arm tests its pattern (`MatchInt`/`MatchStr`/`MatchBool` for literals, `MatchVariant` + `ExtractField` for variants, with wildcard/binding as no-ops/aliases), jumps to the next arm on mismatch, binds in a per-arm scope, evaluates its body, and jumps to the end; a value matching no arm hits `MatchFail` (runtime E0007), reproducing M0's non-exhaustive-match error. Nested patterns (`Ok(order)`, `OrderError.NegativePrice(i)`) recurse through `ExtractField`.

**`?` / `??` (`TryUnwrap` / `Coalesce`).** A runtime `try_classify` mirrors M0's `try_branch` (built-in `Result`/`Option` only). `?` unwraps `Ok(x)`/`some(x)` or early-returns `Err`/`none` from the current frame — the M0 `Unwind::Return` translated to the VM's frame teardown (reusing the `Return` path, so a top-level `?` ends the program). `??` unwraps or jumps to the fallback expression. Both raise E0007 on a non-Result/Option.

**Constructors + `panic` + `next_id`.** `Ok`/`Err`/`some` lower to `MakeEnum` over interned built-in shapes (`builtin_result_option` → bare `Ok(x)`/`none` display); `none` is the one prelude *value*. `panic(msg)` is the `Panic` op (E0010, keeping prior stdout). `next_id()` is a deterministic counter seeded at 1, matching M0's `IdGen` (the demo's `Order #1`/`#2`).

**Capture-free nested closures.** The demo and `construct_and_method` define `fn(it) => it.price * it.qty` *inside* a method. M1.2 rejected all non-top-level closures; this slice allows a nested closure whose free variables are all globals/prelude — a `captures_forbidden` set (the enclosing function's locals/fields/`self`) makes any genuine capture `Unsupported` (true upvalues remain deferred). This was the last blocker to 100%.

**Known conservative gap (safe, non-corpus):** a bare `x = expr` that introduces a *new local inside a function body* is still `Unsupported` (the function compiler can't disambiguate a new local from an outer-global reassignment) — a safe skip, exercised by no corpus case. The clean fix (function-local binding analysis) is left for a later slice; it never causes divergence, only a skip.
