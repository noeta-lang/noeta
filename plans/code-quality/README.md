# Code-quality cleanup

An architectural / code-quality review of the whole codebase (six subsystem
passes) produced a findings list. Most were addressed directly on the
`code-quality-cleanup` branch — the bounded and medium items: doc de-drift,
DRY dedups, dead code, primitive-obsession (concurrency-id newtypes), the
Host god-trait ISP split, the checker god-object's site-map grouping, the
`self.error()` diagnostic helper, the IR `LoweringSites` bundle, and the
`assignable`→`subtype` unification. See that branch's history.

Four findings are **large, entangled, readability-only refactors** that were
deliberately *not* batched into that arc — each is its own multi-hour focused
effort, best done fresh with full context rather than rushed. They are scoped
here, one file each. None is a correctness gap: the differential oracle already
guarantees behavior, so every one of these is a pure structural refactor whose
gate is "differential unchanged (417 matched / 0 skipped, backends agree),
clippy/fmt clean, and — where the unsafe crate is touched — miri clean."

| Track | File | The god file (current LOC) | Shape |
|---|---|---|---|
| Split the VM | [`split-vm-lib.md`](split-vm-lib.md) | `lang-vm/src/lib.rs` (7733) | Extract cohesive `impl Vm` / free-fn clusters into modules; the 2740-line `dispatch` fn is the hard core. |
| Split the checker | [`split-checker-lib.md`](split-checker-lib.md) | `lang-check/src/lib.rs` (5722) | Split one enormous `impl Checker` across `expr`/`decl`/`traits`/`attributes` submodules. |
| Decompose the parser fns | [`split-parser-fns.md`](split-parser-fns.md) | `lang-parser/src/lib.rs` (3679) | Break up the 1164-line `statement_parser` and 744-line `expr_with` chumsky closures. |
| BuiltinTrait enum | [`builtin-trait-enum.md`](builtin-trait-enum.md) | `lang-check` (cross-cutting) | Replace stringly-typed trait/module dispatch with the existing `BuiltinTrait` enum. |

**Recommended order:** parser and VM/checker splits are independent and can be
done in any order; do **BuiltinTrait** before or independently of the checker
split (it touches the same file but different code). Pick the one whose file you
are about to work in anyway — a split pays off most right before feature work in
that area.

**Shared discipline for all four:**

- Pure refactor — **no behavior change**. Gate: `--differential` stays at
  417 matched / 0 skipped / backends agree; `cargo clippy --workspace
  --all-targets` and `cargo fmt --all --check` clean; `miri` clean for any
  unsafe crate touched (only the VM split plausibly touches `lang-value`).
- Prefer the proven **file-extraction pattern** (see `lang-ir/src/lower/state_machine.rs`
  and `lang-parser/src/literals.rs`): move a cohesive cluster to a submodule,
  make the few items the parent still calls `pub(crate)`, and let the submodule
  reach otherwise-private parent items via descendant-module visibility.
- Commit per coherent sub-extraction, not one giant diff — each sub-module move
  should compile and pass the differential on its own.
