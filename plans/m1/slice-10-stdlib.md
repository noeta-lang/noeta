# Slice M1.10 — Layered stdlib (Ring 1 + Ring 2)

Status: todo

## Goal
Ship the always-present standard library: rich Ring 1 core types entangled with the operator traits, and the thin Ring 2 always-shipped modules.

## Scope
- In:
  - **`lang-stdlib`** crate.
  - **Ring 1** (always present): `List`, `Map`, `Set`, ordered/sorted maps+sets, deque/queue; full Unicode-correct strings; numeric primitives; `Option`/`Result`. A Python-generous method surface (map/filter/fold, slicing, iteration, full string ops) bound to the M1.8 operator traits.
  - **Ring 2** (always shipped, thin): file/IO + filesystem (paths/streams); process/environment/args; basic scalar math; basic **seeded** random (general-purpose PRNG); basic time (now/sleep/measure/**monotonic** — no timezone/calendar); JSON.
- Out: Ring 3 (regex, crypto, HTTP client, timezone date/time, YAML/TOML/CSV, compression, 3D/SIMD math, derive-driven Serialize/Deserialize) — all post-M1 via the extension mechanism; async-first IO internals (M2).

## Checklist (vertical slice)
- [ ] Grammar / AST: none (stdlib is library code + native bindings, not syntax).
- [ ] Checker rule: stdlib types carry real signatures the checker enforces; trait impls (Iterable/Index/Display/…) for Ring 1 types.
- [ ] Bytecode: native-call bindings for stdlib functions.
- [ ] VM op: native function dispatch into `lang-stdlib`.
- [ ] Conformance cases: per Ring-1 type (List/Map/Set/string/numeric) and per Ring-2 module (file IO round-trip in a temp dir, env/args read, seeded-random determinism, monotonic-time measure, JSON parse+emit round-trip).
- [ ] Snapshots: rendered diagnostics for stdlib type errors where useful.

## Definition of done
- Ring 1 + Ring 2 APIs implemented, trait-bound, and conformance-covered; determinism gates hold (seeded RNG, sorted iteration, no wall-clock in output).
- **M1 milestone complete:** a real domain program with typed errors, generics, pattern matching, and stdlib use compiles clean and runs on the VM, differential-identical where the tree-walker can express it.
- fmt/clippy clean.

## Notes / traps
- Ring 2 must stay thin (Go-lean), Ring 1 rich (Python-generous) — stdlib breadth is a curation decision; Ring 3 reuses the extension mechanism rather than a separate stdlib-loading path.
- Determinism is load-bearing for the agent feedback loop: no wall-clock, no hash-order, seeded PRNG. Conformance-enforce it.
