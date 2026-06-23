# Slice S3a — Type the stdlib / method / prelude / index surface

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`). **Depends on:** S2. **Prerequisite for:** S3b (the strict flip). **Determinism posture:** adds type *knowledge*; the checker is shared, so any newly-surfaced error is identical on both backends. The gradual corpus tolerates the new types, so conformance + differential stay green (**116 / 110 / 0 skipped**).

## Goal
Give the checker concrete return types for the built-in stdlib surface — string/list/map/set methods, prelude free functions, indexing, user-method returns, and the Ring 2 module calls — so those expressions stop resolving to `Unknown`. Without this, the strict flip (S3b) would reject almost the whole corpus, since it has no idea what `s.upper()` or `len(xs)` returns. This is the heaviest, most faithful path (chosen with the user over a `dyn`-the-rest hybrid): **no `dyn`-by-default creep** — the stdlib is genuinely typed.

## What shipped

### Part 1 — lattice prep (`Type::Set`, dyn-defers rule)
- **`Type::Set(T)`** — the lattice had no set type, so `to_set()` and the set methods could never be typed. Added with covariant subtyping + `Display`.
- **Bare `list`/`map`/`set` annotations** desugar to collections with an **inference-hole** element (`List<Unknown>`, …): "element unspecified", tolerated by the gradual checker now, forced explicit at the strict flip. They become real built-in names (out of the checker's `PRELUDE_TYPES`).
- **`Type::defers_to_runtime()`** = holes **or** `dyn`. Typing the stdlib surfaces real `dyn` values (`json.parse`), and `dyn` is *not* a gradual hole, so the operator/`?` checks would wrongly reject `dyn` operands. They now defer on `dyn` too (its sanctioned dynamic-dispatch semantics), returning the deferred type. Member/index on a `dyn` likewise yields `dyn`.

### Part 2 — the signature table (`lang-check/src/stdlib.rs`)
A new module mirroring the runtime's return types (it lives next to the checker, not in `lang-stdlib`, because the types reference `lang_types::Type` — generics/`Option`/`dyn` — which the stdlib crate does not model). Resolvers:
- `method_return(recv, name)` — string/list/map/set methods + `compare -> Ordering` + file-handle methods (`fs.open`'s `FileHandle` value, a reserved built-in type name).
- `prelude_return(name, args)` — `len`/`next_id` → `int`; `sum` kind-aware; `map`/`filter` from the closure/list arg; `Ok`/`Err`/`some` polymorphic; `panic` diverges.
- `module_return(module, name, args)` — every `json`/`math`/`random`/`fs`/`time`/`env`/`args` function (kind-preserving numerics for `math.abs`/`min`/`max`).
- `index_return(recv)` — `List<T>→T`, `Map<_,V>→V`, `string→string`, `dyn→dyn`.

Wired into the checker: `synth_call` now receives synthesized argument types and routes plain calls → user-fn/prelude, `module.f(...)` → module table, `recv.method(...)` → built-in/user-method/deferred. The checker tracks user-class method return types (`methods` map, from class + `impl` methods) and the names bound to stdlib modules by `use std.{…}` (`modules` set). `Expr::Index` uses `index_return`; member access on `dyn` stays deferred.

## Determinism / oracle posture
New static rejections come from the shared checker → identical on both backends → differential unaffected (**110 / 0 skipped**). The gradual corpus tolerates the new precision (one case, `index/index_trait.lang`, drove the bare-collection-element decision: mapping `list` to `List<dyn>` would have forced `dyn` narrowing on `items[i]` returned as `int`; mapping it to `List<Unknown>` keeps it gradual until S3b migrates such annotations to `List<int>`). Conformance **116 passed**.

## Files
- `crates/lang-types/src/lib.rs` — `Type::Set`, `defers_to_runtime`, bare-collection desugaring, subtype/Display, tests.
- `crates/lang-check/src/stdlib.rs` (new) — the signature table.
- `crates/lang-check/src/lib.rs` — `mod stdlib`, `methods`/`modules` fields, collect population, `synth_call`/`method_call_return`/`synth_member`/`Expr::Index` wiring, dyn-defers in `synth_binary`/`Try`, `PRELUDE_TYPES` trimmed.
- `crates/lang-check/src/tests.rs` — 5 new tests proving string/list/prelude/module/index/user-method results are concretely typed (each only fails if the type flows).

## Definition of done — met
The stdlib surface is concretely typed (proven by white-box tests that only pass if the type flows against an S2 return expectation); 39 checker + 15 type tests pass; conformance 116 / differential 110 / 0 skipped; clippy + fmt clean. The checker now knows enough types for the strict flip (S3b) to be viable.
