# Split the VM (`lang-vm/src/lib.rs`, 7733 LOC)

Status: done (core split) — optional `dispatch` decomposition remains

**Done (`2cc06db` → `28c72b9`):** the three concern seams are extracted —
`methods.rs` (1119 LOC, receiver-method dispatch), `scheduler.rs` (473 LOC,
async/isolate scheduler), `values.rs` (475 LOC, the value-construction seam).
lib.rs dropped **7733 → 5729 LOC**. Extraction pattern: `impl Vm` methods moved
verbatim (methods/scheduler → the few called from lib.rs are `pub(crate)`); the
free-fn value cluster moved to `values.rs` as `pub(crate)` + re-exported at the
crate root (`pub(crate) use values::*`) so callers use bare names unchanged.
Differential 417/0 (backends agree), 118 VM unit tests, clippy/fmt clean;
lang-value untouched so miri unaffected. **Remaining (optional follow-ons, see
Out/Design):** the 2740-line `dispatch` fn → per-opcode `fn op_*`, the `Vm`
25-field regrouping, and `Op::operands()/defs()` to collapse the regalloc
matches.

The register VM lives in one 7733-line file. It is competently engineered
(inline caches, disciplined refcounting, a shared `Host`/`Executor` seam) but
spans several unrelated concerns, and the single `dispatch` fn is 2740 lines.
This track carves the file into modules **without touching behavior**.

## Goal

`lang-vm/src/lib.rs` drops to roughly the core interpreter (`Vm`, `Frame`, the
`dispatch` loop, and `run`), with the value-construction/marshalling helpers,
the async/isolate scheduler, and the builtin-method dispatch living in their own
modules. Target: lib.rs well under half its current size; no module over ~1500
LOC; behavior byte-identical.

## Scope

- **In (extract to modules):**
  - **`values.rs`** — the ~450-line free-function cluster from `marshal_native_arg`
    (line 5909) through `materialize` (6347): the native-registry value seam
    (`marshal_native_arg`/`value_to_scalar`/`scalar_to_value`/`materialize_ext`/
    `materialize_native`/`stdlib_output_to_value`/`materialize_recipe`), the
    reflection value builders (`vm_type_repr`/`build_type_value`/`reflection_type_name`),
    and the general constructors (`make_some`/`make_none`/`make_ordering`/`make_role`/
    `make_attr_enum`/`materialize`/`arity_message`/`stdlib_error_code`).
  - **`scheduler.rs`** — the async/isolate machinery: the `poll_*` methods,
    `spawn_isolate*`, `join_scope`, `cancel_task` (there is already an `isolate.rs`
    seam to grow into).
  - **`methods.rs`** — the builtin/receiver method dispatch cluster
    (`call_*_method`, ~1000 LOC).
- **Out (stays in lib.rs, this pass):**
  - The 2740-line `dispatch` fn (line 2045) and the `Vm`/`Frame` core. Decomposing
    `dispatch` into per-opcode `fn op_*(&mut self, …)` methods is a **separate,
    larger sub-effort** (see Design) — do it only if this pass leaves appetite.
  - The `Vm` 25-field struct regrouping (into `Scheduler`/`IsolateRuntime`/dispatch-
    tables sub-structs) — a nice follow-on, not required for the file split.

## Design

Follow the `state_machine.rs` / `literals.rs` extraction pattern:

1. The `values.rs` cluster is free functions, but it calls the VM's `retain`/
   `release`/`set_reg` (define those `pub(crate)` in lib.rs) and ~15 of its
   members are called from the main body (make those `pub(crate)`). One function
   in the range uses `self` — leave that one in lib.rs (it is a `Vm` method, not a
   free fn). The submodule reaches otherwise-private parent items via
   descendant-module visibility (`use crate::{retain, release, …}`).
2. `scheduler.rs`/`methods.rs` are `impl Vm` methods → add `impl Vm { … }` blocks
   in the new files (same crate ⇒ private-field access is fine). No `pub(crate)`
   needed for methods only called on `self`.
3. Opcode-coupling note (optional stretch): ~90 `Op` variants each require an arm
   in `dispatch`, `op_repr`, `regalloc::op_facts`, and `regalloc::remap_op`.
   A trait-driven `Op::operands()`/`defs()` could auto-derive the regalloc facts,
   collapsing two of the four matches — a worthwhile but independent change.

## Risks & constraints

- The value cluster is **moderately entangled** (parent-helper calls + ~15
  call-backs + one `self`-using fn) — expect iterative compile-fix cycles on the
  `pub(crate)`/`use` surface. This is churn, not risk: the compiler verifies it.
- `lang-value` is the only unsafe crate; this split should not touch it, but if a
  moved helper does, **miri** must stay green.
- Keep each module move a separate commit that compiles + passes the differential.

## Checklist

- [x] `values.rs` extracted; `pub(crate)` re-export at crate root; lib.rs shrinks ~450 LOC
- [x] `scheduler.rs` (async/isolate) extracted
- [x] `methods.rs` (builtin/receiver dispatch) extracted
- [ ] (optional) `dispatch` arms → `fn op_*` methods
- [ ] (optional) `Op::operands()/defs()` to collapse regalloc matches
- [x] differential 417/0, backends agree; clippy `--all-targets` + fmt clean; miri n/a (lang-value untouched)

## Definition of done

`lang-vm/src/lib.rs` is materially smaller and split along the four concern
seams above, each module self-contained; the differential is unchanged and all
gates are green. The `dispatch`-fn decomposition and `Vm`-struct regrouping may
remain as noted follow-ons.
