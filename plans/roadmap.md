# Roadmap

The single source of "what's next / what's done". Milestones are distilled from `docs/resources/03-implementation-plan.md`; the full design rationale lives there and in the other `docs/resources/` docs. Re-scan this file at the start of every work session.

## Milestone overview

| Milestone | Goal | Status |
|---|---|---|
| **M0 — Walking skeleton** | Run simple programs via a tree-walking evaluator; prove the syntax feels right; stand up the test harness and crate seams. | **done** |
| **M1 — Real language core** | Replace tree-walker with register-based bytecode + VM; NaN-boxed values; shape-based object model + inline caches; refcount+cycle GC; type checker + inference (salsa query graph); generics/ADTs/traits/derives; modules; layered stdlib (Ring 1/2). | **in progress** |
| **M2 — Differentiators** | Persistent runtime + isolates; async/structured concurrency + TaskScope; bundled HTTP/WS server; signals; embedded LSP; native toolchain; AOT + DCE; Tier-1 specializing interpreter; HMR; observability; agentic MCP surface; baseline DB; packed value types. | not started |
| **M3 — Long tail** | WASM target; Tauri desktop; background-work extensions; JIT; editor grammars + VS Code ext; reactive persistence; p2p/local-first; extension system + stable host ABI; startup cache; editions. | not started |

Detailed M2–M3 decomposition is deferred to a dedicated planning pass when each milestone is reached. M0 and M1 are sliced below.

## M1 slices

The runtime/VM-first sequence: stand up the bytecode VM and reproduce M0 byte-for-byte (Thrust A), then layer the salsa type checker (Thrust B), then modules + stdlib (Thrust C). Each slice links to its file in `m1/`. Pick the lowest-numbered `todo`.

The differential oracle is the spine: `TreeWalkBackend` (M0, frozen) and the new `VmBackend` are run against the same programs and their `RunResult`s asserted identical via `lang test --differential`. The tree-walker is retained forever as the oracle, never deleted. Until the VM compiles 100% of the corpus, cases it can't yet lower are **skipped** (not failed), tracked as a climbing coverage %.

| # | Slice | Thrust | Status |
|---|---|---|---|
| 0 | [Value spine + minimal VM + differential oracle](m1/slice-00-vm-spine.md) | A | done (31.2% VM corpus coverage) |
| 1 | [Salsa db plumbing](m1/slice-01-salsa-db.md) | A | done (pipeline as salsa 0.27 queries: `tokens → ast → bytecode`; differential unchanged at 100%) |
| 2 | [Functions, calls, closures, pipeline](m1/slice-02-functions.md) | A | done (43.8% VM corpus coverage) |
| 3 | [Heap collections: List + Map](m1/slice-03-collections.md) | A | done (62.5% VM corpus coverage; string interpolation `Expr::Interp` landed alongside) |
| 4 | [Shapes + objects + enums (object model)](m1/slice-04-shapes-objects.md) | A | done (75.0% VM corpus coverage) |
| 5 | [Result/Option/`?`/`??` + match](m1/slice-05-result-match.md) | A | **done (100% VM corpus coverage — Thrust A gate met)** |
| 6 | [GC cycle collector + `__destruct`](m1/slice-06-gc.md) | A | done (deterministic `destruct` in both backends + trial-deletion cycle collector; `gc-arena` tracing path deferred — see slice) |
| 7 | [Type checker: types + inference + ADT/exhaustiveness + ownership](m1/slice-07-checker.md) | B | done (gradual checker as a shared front-end: inference + exhaustiveness E0011 + `?`-typing E0012 + arithmetic mismatch E0007; unknown-type E0013 deferred to M1.9, ownership/immutability to 7b) |
| 8 | [Traits as operators + derives + generics](m1/slice-08-traits.md) | B | in progress (8a done: `+ - * / ~` overloading via `impl` + `#[derive]`/attribute surface + E0014/E0015; 8b underway: `Equatable` `==`/`!=` + fallible `TryAdd` done; todo: `Comparable`/`Ordering`, derive codegen, other protocols, generics, manifest) |
| 9 | [Modules / namespaces / `use` resolution](m1/slice-09-modules.md) | C | todo |
| 10 | [Layered stdlib (Ring 1 + Ring 2)](m1/slice-10-stdlib.md) | C | todo |

**Thrust A gate (✅ met at M1.5):** `VmBackend` runs 100% of the M0 corpus, every case differential-identical to the tree-walker — including the full §14 demo. The tree-walker is now frozen as the pure oracle; the conformance suite asserts `skipped == 0`, so any new feature must land in both backends (or be explicitly oracle-exempt). **Thrust B gate:** every static-error class has a negative conformance case. New `unsafe` is quarantined to `lang-value`/`lang-gc`/`lang-vm`, miri-gated.

## M0 slices

Each links to its file in `m0/`. Pick the lowest-numbered `todo`.

| # | Slice | Status |
|---|---|---|
| 0 | [Skeleton + diagnostics spine + hairline end-to-end + harness](m0/slice-00-skeleton.md) | done |
| 1 | [Bindings, literals, arithmetic, `~` concat](m0/slice-01-bindings.md) | done |
| 2 | [Functions, closures, calls, pipeline `\|>`](m0/slice-02-functions.md) | done |
| 3 | [Control flow + collections](m0/slice-03-control-flow.md) | done |
| 4 | [String interpolation](m0/slice-04-interpolation.md) | done |
| 5 | [`match` + enums](m0/slice-05-match-enums.md) | done |
| 6 | [Records & classes](m0/slice-06-records-classes.md) | done |
| 7 | [`Result`/`Option`/`?`](m0/slice-07-result-option.md) | done |
| 8 | [`namespace` / `use`](m0/slice-08-namespace-use.md) | done |
| 9 | [§14 demo + REPL + proptest](m0/slice-09-demo-proptest.md) | done |

### Post-M0 hardening (after Slice 9, before M1)

A test-hardening pass closed coverage gaps the slices left (commits `5b285a5`, `539daba`):

- **New test suites:** `crates/lang-cli/tests/cli.rs` (subprocess-driven `run`/`repl`/`test` end-to-end via `assert_cmd`), `crates/lang-eval/tests/diagnostics.rs` (rendered-`ariadne` snapshot gallery for E0001–E0010), plus expanded `value.rs`/`ops.rs` unit tests.
- **Real bug fixed:** the lexer string regex `"[^"]*"` couldn't span an escaped quote (`\"` terminated the string early); fixed to `"([^"\\]|\\.)*"`, with a regression test.
- **Corpus grew 22 → 36 cases; test functions 88 → 120.**
- **Coverage tooling switched to `cargo-llvm-cov`** (tarpaulin uninstalled — it can't see across the subprocess boundary, reporting the CLI tests' `lang` binary as 0%). Baseline: **87.10% lines / 89.40% regions / 92.36% functions**. See `AGENTS.md` Testing.

## Standing requirements (every slice)

- **Tests grow continuously.** Every slice adds conformance cases (the iron rule). Coverage of the feature set by the corpus is tracked here as slices complete.
- **Benchmarks.** `criterion` perf-regression gates are M1+ (no hot VM path to guard in M0); `benches/` is reserved now. When the M1 VM lands, every VM-touching slice adds/maintains a bench over the hot paths (dispatch loop, property access through inline caches, allocation).
- **Determinism.** No time-, hash-order-, or thread-scheduling-dependent output. Seed RNGs, sort map iteration in test mode, `next_id()` is a seeded counter.
- **Zero `unsafe` in M0.** `#![forbid(unsafe_code)]` in every crate; the first `unsafe` appears with the M1 `vm`/`gc` crates, quarantined and `miri`-checked.

## M0 definition of done

See the bottom of each slice and the approved plan. M0 is complete when the syntax-doc §14 program runs end-to-end via `lang run`, a REPL and file runner work, every surface feature has passing conformance cases (incl. error cases), the layered harness (`lang test` with `--json`/`--file`/`--stage`) and the `Backend`/`RunResult` differential seam exist, proptest properties run, diagnostics are centralized with stable codes, and the workspace is clean under fmt/clippy with zero `unsafe`.
