# Roadmap

The single source of "what's next / what's done". Milestones are distilled from `docs/resources/03-implementation-plan.md`; the full design rationale lives there and in the other `docs/resources/` docs. Re-scan this file at the start of every work session.

## Milestone overview

| Milestone | Goal | Status |
|---|---|---|
| **M0 — Walking skeleton** | Run simple programs via a tree-walking evaluator; prove the syntax feels right; stand up the test harness and crate seams. | in-progress |
| **M1 — Real language core** | Replace tree-walker with register-based bytecode + VM; NaN-boxed values; shape-based object model + inline caches; refcount+cycle GC; type checker + inference (salsa query graph); generics/ADTs/traits/derives; modules; layered stdlib (Ring 1/2). | not started |
| **M2 — Differentiators** | Persistent runtime + isolates; async/structured concurrency + TaskScope; bundled HTTP/WS server; signals; embedded LSP; native toolchain; AOT + DCE; Tier-1 specializing interpreter; HMR; observability; agentic MCP surface; baseline DB; packed value types. | not started |
| **M3 — Long tail** | WASM target; Tauri desktop; background-work extensions; JIT; editor grammars + VS Code ext; reactive persistence; p2p/local-first; extension system + stable host ABI; startup cache; editions. | not started |

Detailed M1–M3 decomposition is deferred to a dedicated planning pass when each milestone is reached. Only M0 is sliced below.

## M0 slices

Each links to its file in `m0/`. Pick the lowest-numbered `todo`.

| # | Slice | Status |
|---|---|---|
| 0 | [Skeleton + diagnostics spine + hairline end-to-end + harness](m0/slice-00-skeleton.md) | done |
| 1 | [Bindings, literals, arithmetic, `~` concat](m0/slice-01-bindings.md) | todo |
| 2 | [Functions, closures, calls, pipeline `\|>`](m0/slice-02-functions.md) | todo |
| 3 | [Control flow + collections](m0/slice-03-control-flow.md) | todo |
| 4 | [String interpolation](m0/slice-04-interpolation.md) | todo |
| 5 | [`match` + enums](m0/slice-05-match-enums.md) | todo |
| 6 | [Records & classes](m0/slice-06-records-classes.md) | todo |
| 7 | [`Result`/`Option`/`?`](m0/slice-07-result-option.md) | todo |
| 8 | [`namespace` / `use`](m0/slice-08-namespace-use.md) | todo |
| 9 | [§14 demo + REPL + proptest](m0/slice-09-demo-proptest.md) | todo |

## Standing requirements (every slice)

- **Tests grow continuously.** Every slice adds conformance cases (the iron rule). Coverage of the feature set by the corpus is tracked here as slices complete.
- **Benchmarks.** `criterion` perf-regression gates are M1+ (no hot VM path to guard in M0); `benches/` is reserved now. When the M1 VM lands, every VM-touching slice adds/maintains a bench over the hot paths (dispatch loop, property access through inline caches, allocation).
- **Determinism.** No time-, hash-order-, or thread-scheduling-dependent output. Seed RNGs, sort map iteration in test mode, `next_id()` is a seeded counter.
- **Zero `unsafe` in M0.** `#![forbid(unsafe_code)]` in every crate; the first `unsafe` appears with the M1 `vm`/`gc` crates, quarantined and `miri`-checked.

## M0 definition of done

See the bottom of each slice and the approved plan. M0 is complete when the syntax-doc §14 program runs end-to-end via `lang run`, a REPL and file runner work, every surface feature has passing conformance cases (incl. error cases), the layered harness (`lang test` with `--json`/`--file`/`--stage`) and the `Backend`/`RunResult` differential seam exist, proptest properties run, diagnostics are centralized with stable codes, and the workspace is clean under fmt/clippy with zero `unsafe`.
