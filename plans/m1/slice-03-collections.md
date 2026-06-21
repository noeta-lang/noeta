# Slice M1.3 — Heap collections: List + Map

Status: done

## Goal
List and Map as refcounted heap objects, with the prelude collection builtins and `for`-iteration, reproducing M0's deterministic display exactly.

## Scope
- In: `Expr::List`/`Expr::Map` lowering; heap `List`/`Map` objects with refcounted elements; `len`/`map`/`filter`/`sum` builtins (as VM ops or native calls); `for … in` over list/map; `(i, x)` destructuring from `.enumerate()`; `.count()`; deterministic sorted-key `Map` iteration and the exact `format_float` display (trailing `.0`). Stress-allocation test mode over the corpus.
- Out: Set/deque (M1.10 Ring 1); shape-based objects (M1.4); type-checked element types/generics (M1.7/M1.8).

## Checklist (vertical slice)
- [x] Grammar / AST: none (reuses M0 `List`/`Map`/`For`).
- [x] Checker rule: n/a (M1.7).
- [x] Bytecode: list/map construct + index/iterate opcodes; native-call convention for `map`/`filter`/`sum`.
- [x] VM op: heap list/map ops, iteration protocol, builtin dispatch (`lang-vm` + `lang-gc` for element refcounts).
- [x] Conformance cases: existing `collections/*.lang`, `control_flow/*.lang` cases run on `VmBackend`.
- [x] Snapshots: disassembly for a `for`-loop and a `map`/`filter` chain.

## Definition of done
- [x] All M0 collection + control-flow corpus cases differential-identical on `VmBackend`.
- [x] Stress-allocation mode (aggressive alloc/free) over the corpus surfaces no use-after-free; miri green.
- [x] fmt/clippy clean.

## Notes / traps
- The differential oracle will instantly catch any drift in sorted-map display or float formatting — expected, that's the oracle working. Match M0's `BTreeMap` ordering and `format_float` precisely.
- First real GC pressure lands here; the cycle collector is still M1.6, so only acyclic collection garbage is reclaimed (by refcount). Document any retained cyclic case.

## Outcome

Landed the heap collections end-to-end, lifting VM corpus coverage **43.8% → 56.2%** (14 → 18 cases matched), zero divergence. The four newly-covered cases: `collections/list_map_pipeline`, `collections/empty_and_trailing_commas`, `control_flow/if_for`, `control_flow/nested`.

**Value representation (`lang-value`).** Two new heap payloads, `Payload::List(Vec<Value>)` and `Payload::Map(BTreeMap<String, Value>)`. A collection **owns one reference to each value it holds**; `heap::free` releases those children (recursively) before dropping the container's allocation, so a list of strings — or a list of lists — frees cleanly. Display mirrors M0 exactly: elements render via a new `Value::repr` (strings quoted inside a collection), maps iterate in sorted-key order, and `format_float` is unchanged. `type_name` gains `"list"`/`"map"`.

**Bytecode (`lang-bytecode`).** New ops: `MakeList`, `MakeMap` (+ `RequireMapKey`, which checks each key is a string *before* its value is evaluated, matching M0's per-entry error timing), `IterSnapshot`/`ListLen`/`ListGet`/`DestructurePair` (the `for` iteration protocol), and `CallBuiltin`/`CallMethod`. Two small enums, `Builtin {Len,Map,Filter,Sum}` and `Method {Count,Enumerate}`, carry the dispatch tag; builtins are **not** first-class values in this slice (a program that passes one around rather than calling it stays unsupported), so they ride in a dedicated op rather than a register.

**Lowering (`lang-compiler`).** `Expr::List`/`Expr::Map` build the collection from evaluated registers; `Stmt::For` lowers to an index loop over an `IterSnapshot` (a list snapshot of the iterable's elements, or a map's values in key order), with `(i, x)` handled by `DestructurePair`. A call whose callee resolves to a prelude `len`/`map`/`filter`/`sum` becomes a `CallBuiltin` (a user binding of the same name shadows it, falling back to an ordinary call); a zero-arg `.count()`/`.enumerate()` becomes a `CallMethod`. Other prelude names (`Ok`/`some`/`panic`/…) remain unsupported, so programs using them are still skipped.

**Execution (`lang-vm`).** The dispatch loop was refactored onto a `Vm` struct (shared `globals`/`stdout`/`diagnostics`) whose frame stack is a *local* of `run`, not a field — so the native `map`/`filter` builtins re-enter the VM to call their closure argument per element (`call_value` runs a fresh single-frame stack to completion) as ordinary Rust recursion. Refcount discipline extends to collection elements: `MakeList`/`MakeMap`/`IterSnapshot`/`ListGet`/`DestructurePair`/`enumerate` retain into the new owner, and an aborting `map`/`filter` releases its partially-built result list before unwinding. `miri` is green over the value/gc/vm suites (including a divide-by-zero-mid-`map` test that exercises the partial-free path and a nested-list round-trip).

**Conservative skips (kept faithful):** nested collections inside cyclic structures can't be built in this subset, so no cycle is retained (the cycle collector is still M1.6); method calls other than `count`/`enumerate`, pipeline-into-method, and bare member field access remain unsupported pending the object model (M1.4).
