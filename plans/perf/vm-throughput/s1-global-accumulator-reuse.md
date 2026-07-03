# S1 — Global-accumulator in-place reuse (P-VMT-GACC)

**Goal.** Remove the **O(n²) cliff** on the most natural scripting idiom: building a collection in a
top-level `mut` accumulator. `mut m = {}` at module scope makes every `m[k] = v` deep-copy the whole
`BTreeMap`; the identical loop inside a function is O(n).

## Evidence

Map build, `m["key${i}"] = i` in a loop:

| n | top-level `m` (global) | `m` inside a fn (local) |
|--:|--:|--:|
| 10k | 1.9 s | — |
| 40k | **33 s** | **42 ms** (~800×) |
| 80k | 152 s (O(n²), flat ns/n²) | — |

Instrumentation (temporary, reverted) proved: the IR reuse pass **does** mark the self-update
(`thread_reuse` → "REUSE-MARK method self-update on 'm'"), and the VM **has** the fast path
(`map_update_in_place`, refcount==1). The mark is dropped by the **compiler**:

```rust
// crates/lang-compiler/src/lib.rs, lower_method (~line 2750)
let recv_reuse = reuse
    && matches!(receiver, Atom::Var { name, .. }
        if matches!(self.resolve(name), Resolved::Local(_)));   // <-- Local ONLY
```

Global receivers fall through to `reuse: false` → the copying `CallMethod` path → deep-copy per
insert. The comment even flags it: *"A global accumulator is the `TakeGlobal` case (a later slice…)."*
This is that slice.

## Approach

`lower_set_field` **already solves the identical problem** for field assignment (`x.f = v` on a
global): it moves the global out with `TakeGlobal` so the in-place op sees refcount 1, then
re-`StoreGlobal`s the result. Mirror that shape in `lower_method` for the collection self-update
methods (`set`/`remove`/`add`) when the receiver resolves to a top-level global:

1. Extend the `recv_reuse` gate to also accept `Resolved::Global` receivers.
2. On the global path, emit `TakeGlobal` (move the sole reference out, leaving `unit`) before the
   in-place `CallMethod { reuse: true }`, then `StoreGlobal` the result back — exactly the
   `lower_set_field` global branch.
3. Order the `TakeGlobal` **after** the args are resolved (a `TakeGlobal` must not vacate a slot the
   args still read — same constraint `lower_set_field` documents).

The VM side already works: `map_update_in_place` / `list_set_in_place` / `set_update_in_place` fire on
`reuse && refcount==1`, and `TakeGlobal` delivers refcount 1.

## Files

- `crates/lang-compiler/src/lib.rs` — `lower_method` reuse gate + global `TakeGlobal`/`StoreGlobal`
  emission (model: `lower_set_field`, same file).

## Validation

- **Benchmark:** parameterized criterion bench (`vm.rs`) — map/list/set build over n ∈ {1k…64k} at
  **top level**, asserting linear scaling (the existing P-COW list bench is the template). Before:
  O(n²); after: O(n). Record the n=40k number (target: ~40 ms, matching the in-fn path).
- **Oracle:** invisible to `RunResult` (reuse is a pure optimization; the eval tree-walker already
  reuses globals, so this brings the VM to parity) → differential stays `0 skipped / agree`. Add a
  top-level-accumulator conformance case if one isn't already present, so the path is exercised.

## Risk

Low, but soundness-sensitive: the `TakeGlobal` ordering vs argument evaluation is the one correctness
constraint (already understood and handled in `lower_set_field`). The runtime refcount check is the
backstop — a wrong mark costs a copy, never a bug.

## Dependencies

None. Land second (right after S0) — smallest change, largest user-visible payoff in the arc.
