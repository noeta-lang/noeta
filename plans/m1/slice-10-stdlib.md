# Slice M1.10 — Layered stdlib (Ring 1 + Ring 2)

Status: **in progress** (M1.10.1 Ring 1 string surface done; M1.10.2 list/map/set + M1.10.3 Ring 2 remaining)

## Goal
Ship the always-present standard library: rich Ring 1 core types entangled with the operator traits, and the thin Ring 2 always-shipped modules.

## Architecture (decided)

`lang-stdlib` is the home of the standard library. Its load-bearing idea: **where a Ring 1 operation is expressible over data represented *identically* in both runtimes, its semantics live in `lang-stdlib` once and both backends call into it — so the differential oracle holds by construction, not merely by test.** Strings are the canonical case: the M0 tree-walker (`Value::Str(String)`) and the M1 VM (`Payload::Str(String)`) both store a Rust `String`, so the entire string-method surface (behavior + arity + argument typing) is defined in `lang-stdlib` and each backend is reduced to thin value↔primitive glue (`Arg`/`Output`/`Dispatch`). Collection methods (list/map) manipulate backend-specific value representations and so are implemented per backend, with the differential as the guard.

Dispatch reuses the existing built-in-method site in both backends (the one that already handles `count`/`enumerate`) — **no compiler or bytecode changes**: a method call is already lowered generically and resolved at runtime.

## M1.10.1 — done (Ring 1 string surface)

- [x] **`lang-stdlib` crate.** `string_method(recv, method, &[Arg]) -> Dispatch` is the single source of truth for the Ring 1 string surface: `upper`/`lower`/`trim` (arity 0), `contains`/`starts_with`/`ends_with` (→ bool), `split` (→ list, empty-separator splits into Unicode scalars), `replace` (2 args), `repeat` (int arg, negative clamps to empty). `Arg`/`Output` are the backend-agnostic projection/lift types; `StdError`/`ErrorKind` carry a rendered message + a code category (both → `E0007 TypeMismatch`). Unicode-correct (`to_uppercase`/`chars`). Crate unit tests cover every method, the empty-separator and negative-repeat edges, Unicode, and both misuse kinds.
- [x] **Both backends wired as thin glue.** `lang-eval` (`call_method`) and `lang-vm` (`Op::CallMethod`) each project args onto `Arg`, call `string_method`, and lift `Output` back into their `Value` — identical by construction. `Dispatch::Unknown` falls through to the existing collection methods (so `"abc".count()` is unaffected). No `unsafe` added.
- [x] **Conformance:** `strings/methods.lang` (every method, incl. Unicode `upper` and quoted-list `split` rendering) and `strings/method_arity_error.lang` (misuse is a runtime `E0007` at the call span, raised identically in both backends — the differential's negative path). Suite **72 passed**; differential **66 matched / 0 skipped / 100% / zero divergence**.

## Scope
- In:
  - **`lang-stdlib`** crate.
  - **Ring 1** (always present): `List`, `Map`, `Set`, ordered/sorted maps+sets, deque/queue; full Unicode-correct strings; numeric primitives; `Option`/`Result`. A Python-generous method surface (map/filter/fold, slicing, iteration, full string ops) bound to the M1.8 operator traits.
  - **Ring 2** (always shipped, thin): file/IO + filesystem (paths/streams); process/environment/args; basic scalar math; basic **seeded** random (general-purpose PRNG); basic time (now/sleep/measure/**monotonic** — no timezone/calendar); JSON.
- Out: Ring 3 (regex, crypto, HTTP client, timezone date/time, YAML/TOML/CSV, compression, 3D/SIMD math, derive-driven Serialize/Deserialize) — all post-M1 via the extension mechanism; async-first IO internals (M2).

## Checklist (vertical slice)
- [x] Grammar / AST: none (stdlib is library code + native bindings, not syntax).
- [ ] Checker rule: stdlib types carry real signatures the checker enforces; trait impls (Iterable/Index/Display/…) for Ring 1 types. *(Gradual checker passes stdlib method calls today; signatures land with the richer surface.)*
- [x] Bytecode: none needed for the method surface — method calls lower generically and resolve at runtime (the `count`/`enumerate` dispatch site). Native *free-function* bindings (Ring 2) will need bytecode work.
- [x] VM op: string-method dispatch routes through `lang-stdlib` at the existing `Op::CallMethod` site (M1.10.1). Collection/Ring-2 dispatch to follow.
- [ ] Conformance cases: string surface done (`strings/methods.lang`, `strings/method_arity_error.lang`); list/map/set + Ring-2 modules (file IO round-trip in a temp dir, env/args read, seeded-random determinism, monotonic-time measure, JSON parse+emit round-trip) remain.
- [ ] Snapshots: rendered diagnostics for stdlib type errors where useful.

## M1.10.2 — todo (Ring 1 list/map/set)
- [ ] List: `reverse`, `contains`, `join`, `sorted`, `slice`, `first`/`last` (→ Option); Map: `keys`, `values`, `has`. Per backend (Value-specific), differential as guard; share Value-agnostic helpers where possible (e.g. `join` via a display callback). Possibly introduce `Set`.

## M1.10.3 — todo (Ring 2 modules)
- [ ] Namespaced native modules. Differential-safe first: `json` (parse/emit), scalar `math`, seeded `random`. **Open design question:** file IO and time interact with the differential oracle (both backends would perform the same side effects / read the same non-deterministic clock) — resolve how the harness sandboxes/threads these before building them. Surface to the user at that point.

## Definition of done
- Ring 1 + Ring 2 APIs implemented, trait-bound, and conformance-covered; determinism gates hold (seeded RNG, sorted iteration, no wall-clock in output).
- **M1 milestone complete:** a real domain program with typed errors, generics, pattern matching, and stdlib use compiles clean and runs on the VM, differential-identical where the tree-walker can express it.
- fmt/clippy clean.

## Notes / traps
- Ring 2 must stay thin (Go-lean), Ring 1 rich (Python-generous) — stdlib breadth is a curation decision; Ring 3 reuses the extension mechanism rather than a separate stdlib-loading path.
- Determinism is load-bearing for the agent feedback loop: no wall-clock, no hash-order, seeded PRNG. Conformance-enforce it.
