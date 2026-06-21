# Slice M1.3 — Heap collections: List + Map

Status: todo

## Goal
List and Map as refcounted heap objects, with the prelude collection builtins and `for`-iteration, reproducing M0's deterministic display exactly.

## Scope
- In: `Expr::List`/`Expr::Map` lowering; heap `List`/`Map` objects with refcounted elements; `len`/`map`/`filter`/`sum` builtins (as VM ops or native calls); `for … in` over list/map; `(i, x)` destructuring from `.enumerate()`; `.count()`; deterministic sorted-key `Map` iteration and the exact `format_float` display (trailing `.0`). Stress-allocation test mode over the corpus.
- Out: Set/deque (M1.10 Ring 1); shape-based objects (M1.4); type-checked element types/generics (M1.7/M1.8).

## Checklist (vertical slice)
- [ ] Grammar / AST: none (reuses M0 `List`/`Map`/`For`).
- [ ] Checker rule: n/a (M1.7).
- [ ] Bytecode: list/map construct + index/iterate opcodes; native-call convention for `map`/`filter`/`sum`.
- [ ] VM op: heap list/map ops, iteration protocol, builtin dispatch (`lang-vm` + `lang-gc` for element refcounts).
- [ ] Conformance cases: existing `collections/*.lang`, `control_flow/*.lang` cases run on `VmBackend`.
- [ ] Snapshots: disassembly for a `for`-loop and a `map`/`filter` chain.

## Definition of done
- All M0 collection + control-flow corpus cases differential-identical on `VmBackend`.
- Stress-allocation mode (aggressive alloc/free) over the corpus surfaces no use-after-free; miri green.
- fmt/clippy clean.

## Notes / traps
- The differential oracle will instantly catch any drift in sorted-map display or float formatting — expected, that's the oracle working. Match M0's `BTreeMap` ordering and `format_float` precisely.
- First real GC pressure lands here; the cycle collector is still M1.6, so only acyclic collection garbage is reclaimed (by refcount). Document any retained cyclic case.
