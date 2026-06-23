# Slice S3b — Argument checking + flip the concrete corpus cases

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`). **Depends on:** S3a (stdlib types). **Determinism posture:** new static rejections from the shared checker → identical on both backends → differential unaffected (**110 / 0 skipped**). The flipped corpus cases stay green transparently (the `error E0007 at L:C` + `exit 1` expectations are agnostic to compile-vs-runtime).

## Goal
With the stdlib typed (S3a), light up the concrete checks the corpus's runtime-`E0007` cases need and add general call-argument soundness — **without** the absolute "every hole errors" endpoint (deferred to S3c, since the naive flip rejects valid polymorphic values like `return none`). Staged scope chosen with the user.

## What shipped

### Part 1 — heterogeneous lists + non-indexable primitives
- **Heterogeneous list literal**: in *synthesis* position, concretely-incompatible elements (`[1, "two"]`) are a static `E0007` (`unify_element` unifies items left-to-right; a deferred element is compatible with anything, two numerics promote to `float`). A mixed list is written explicitly as `List<dyn>`, where the checker arrives through `check` element-by-element instead. Flips `sorted_unorderable`, `to_set_unorderable`.
- **Non-indexable primitive**: indexing a concrete `int`/`float`/`bool`/`unit` (`42[0]`) is a static `E0007`; `Named` types may impl `Index`, and holes/`dyn` defer. Flips `index_non_indexable`.

### Part 2 — call-argument checking (arity + types)
- **Parameter signatures** added to `stdlib.rs` (`method_params`, `module_params`) and tracked for user callables: `FnSig` now carries `params` (top-level functions), and the `methods` map stores a full `FnSig` per user method **with the owning class's generic parameters erased to `dyn`** (`erase_type_params`), so `box.set(5)` on a `Box<T>` is not a false positive.
- **`check_args`**: arity mismatch → `E0007` ("expects N arguments"); per-argument type mismatch → `E0007` ("argument of type X is not assignable to Y"), reported at the callee span. `arg_compatible` is lenient where either side defers (`dyn`/hole) and on numeric widening (`int` accepted for a `float` parameter), so dynamic and numeric calls are not false positives. Flips `method_arity_error` (`upper("extra")`), `list_method_error` (`join(5)`), `math_type_error` (`sqrt("nope")`).
- **Pipeline correctness**: `synth_piped` threads the piped value as the right-hand call's first argument (`5 |> add(10)` is `add(5, 10)`), so pipeline calls are arity-checked correctly and now carry the call's result type.

## Traps handled
- **Pipeline arity**: a piped call has one fewer *explicit* argument; handled by `synth_piped` prepending the piped type.
- **Generic method params**: erased to `dyn` at signature-collection time.
- **`sum`/`math.abs|min|max` of an un-inferred element**: the fallback is a numeric **hole** (`Unknown`), not `dyn` — a missing element type is gradual (resolvable), not the dynamic escape; returning `dyn` would wrongly fail to flow into a concrete `float` return (caught via `demo/orders.lang` / `construct_and_method.lang`).

## Files
- `crates/lang-check/src/stdlib.rs` — `method_params`/`module_params`; `sum`/`numeric_preserving` hole fallback.
- `crates/lang-check/src/lib.rs` — `FnSig.params`, `methods: FnSig`, generic erasure on collect, `synth_piped`, `check_method_args`/`check_args`, `arg_compatible`, `erase_type_params`, heterogeneous-list + non-indexable checks.
- `crates/lang-check/src/tests.rs` — 8 new tests (heterogeneous list, non-indexable, arity, arg types + numeric widening, generic-method leniency, pipeline threading).

## Determinism / oracle posture
All five flipped cases stay green transparently. Conformance **116 passed**, differential **110 matched / 0 skipped / backends agree**. The value/parse runtime errors (`random.int(6,1)`, malformed JSON) correctly stay runtime — not statically knowable.

## Definition of done — met
Heterogeneous lists, non-indexable primitives, and call arity/argument types are statically checked; the five concrete corpus cases flipped to compile-time; 45 checker tests (8 new); conformance 116 / differential 110 / 0 skipped; clippy + fmt clean. The absolute hole-elimination endpoint (`E0023`) remains S3c.
