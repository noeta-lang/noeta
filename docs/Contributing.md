# Contributing & Developer Guide

This page is the developer's entry point to *building* the language — the compiler, not programs in it. It complements the in-repo `AGENTS.md` (the exhaustive conventions reference) and `ARCHITECTURE.md` (the technical overview); this page orients you and points at them.

Noeta is a pre-1.0 implementation — public, with tagged releases — built primarily through agentic engineering, so the discipline below is written to be followed by humans and AI agents alike.

## Orientation

| To understand… | Read… |
|---|---|
| What the language is | [Home](Home) and the [Language Tour](Language-Tour) |
| How the implementation is structured | [Architecture & Pipeline](Architecture-and-Pipeline), and `ARCHITECTURE.md` in the repo |
| The full conventions & agent workflow | `AGENTS.md` in the repo |
| The individual subsystems | the [Concepts & design](Home#concepts--design) pages |
| What to work on next | `plans/roadmap.md` (the frontier) and `plans/backlog.md` (every open item) |

## Build & run

```sh
cargo build                                  # build the workspace + the `noeta` binary
cargo test                                   # unit + snapshot + conformance + property tests
cargo run -p noeta-cli -- run file.noe       # run a program (or use ./target/debug/noeta)
cargo run -p noeta-conformance -- --differential   # the differential oracle (dev harness)
cargo run -p noeta-conformance -- --file hello.noe # one conformance fixture — `--file` matches by
                                                   # path suffix (e.g. `--file gc/class_self_cycle.noe`)
```

You need a recent stable Rust (1.95+).

## The compilation pipeline

Source flows through sharp, single-purpose crates: lexer → parser → checker → IR → RC passes → two backends. Each stage is a separate crate with explicit input/output types and no hidden shared mutable state, so a change is local to one crate and verifiable by that crate's snapshots. The full diagram and crate map are on [Architecture & Pipeline](Architecture-and-Pipeline). Every crate carries a `README.md` as its primary documentation — start there when working in one.

## The differential oracle is the spine

The language is implemented twice — a reference interpreter (`noeta-eval`) and a bytecode VM (`noeta-vm`) — and the conformance harness asserts their observable output is byte-for-byte identical on every corpus program, with a hard **`0 skipped`** gate. This is the backbone of the test strategy. Two rules follow:

1. **A feature lands in both backends** (or is explicitly oracle-exempt). Shared semantics go in `noeta-stdlib` so the two agree by construction rather than by luck.
2. **The differential must stay green** — `--differential` at 0 skipped, backends agree — after every change.

## The new-feature template

A language feature is added as a **vertical slice**, in this order:

1. **Grammar / AST** — token(s) in `noeta-lexer`, node(s) in `noeta-ast`, production in `noeta-parser` (keep surface sugar as its own AST node).
2. **Checker rule** — typing/inference in `noeta-check` (+ a new `Type` form in `noeta-types` if needed); add a negative conformance case for any new static-error class.
3. **Lowering** — AST → IR in `noeta-ir` (+ `noeta-ir-passes` if the feature introduces owned heap values needing drops/reuse).
4. **Both backends** — evaluation in `noeta-eval` **and** bytecode in `noeta-compiler`/`noeta-vm`; keep shared semantics in `noeta-stdlib`.
5. **Conformance cases** — `tests/conformance/**.noe` with `// expect:` headers, including error cases; must run `--differential` at 0 skipped.
6. **Snapshot update** — review `insta` snapshots deliberately; never blind-accept.

> **The iron rule: every feature or fix lands with a conformance corpus entry.** Prefer vertical-slice tasks ("implement `~` end to end") over diffuse refactors — a slice's done-condition is "its conformance cases pass."

## Testing architecture

- **Per-stage snapshots** (`insta`) — tokens, AST, and rendered diagnostics are pinned at each boundary.
- **Conformance corpus** (`tests/conformance/**.noe`) — executable end-to-end behavior with `// expect:` headers, run through both backends and asserted identical.
- **Property tests** (`proptest`) — invariants like parse→print→parse round-trips, and the static-≤-dynamic last-use property for the RC passes.
- **Structured fuzzing** (`noeta-fuzz`) — a generator that turns a `&[u8]` into a *syntactically valid* Noeta program, so the properties under test are evaluated past the parser rather than bouncing off it. Byte-level mutation is near-useless against a language front-end: almost every mutated buffer fails to lex, the component declines it, and every property passes vacuously. Generation is total — bytes are consumed left to right, and once they run out every further choice reads as `0`, with each choice list ordered so `0` is the most terminal alternative — so exhaustion winds a program down instead of truncating it mid-token, and any buffer yields something parseable. That contract is what lets one generator serve both a seeded deterministic sweep and a `proptest` driver that shrinks the *driver bytes* (fewer bytes, smaller program) into a paste-ready reproducer.
- **What the fuzzer adds over the corpus.** Every corpus file has exactly one layout: the one its author wrote and `noeta fmt` already normalized. The generator varies what a corpus of any size barely samples — nesting, collapsed versus exploded block bodies, blank-line runs, semicolon presence, header parentheses, method-chain breaks, comment placement at every depth, and the whole config space rather than the three configurations the corpus pins. Its first run over `noeta fmt` found six printer defects, none reachable from the corpus, three of which relocated or deleted a comment under the *default* config.
- **Keeping a structured fuzzer honest.** This is the way it fails silently: drift into emitting unparseable text and the suite stays green while testing nothing. So the generator's parse rate is asserted, not merely reported (`tests/generator.rs`), alongside totality over degenerate buffers and determinism, which reproduction from a reported seed depends on. Any bounded sweep also states its bound — `SEEDS` in `tests/fmt.rs` and `NONCES` in `tests/run.rs` and `tests/project.rs` are floors, not claims, and `triage scan 50000` / `runscan scan 120000` / `projectscan scan 20000` run the identical oracles as deep as you care to wait. Every oracle carries a *reach* assertion for the same reason: the formatter's is the parse rate, the `.noeb` container's is how many corrupted bundles get past the header, the execution oracle's is how many generated programs actually run (~7% — most are rejected by the checker, legitimately), and the project differential's is how many generated projects *each* front-end accepted, floored on both sides because its invariant is a boolean and a population that only ever takes one value proves one implication and skips the other. Without one, a sweep that tests nothing is indistinguishable from a sweep that finds nothing.
- **The execution oracle** (`noeta-fuzz`, `tests/run.rs`) — the same generator pointed at the pipeline that runs a program, asserting three things the tree already claims: a checked program compiles (`compile_real`'s own doc calls an `Err` there an internal invariant break), a checked program does not fail *statically* at run time (the check-vs-run divergence class), and the two backends agree. It found six checker gaps in its first sweeps — a hoisted global visible to the statement that binds it (`a = a`), a type name accepted in callee position (`S()`), a `for (a, b)` pattern over a non-tuple element, an ordering between `int` and `bool`, missing methods on function values and tuples, and a duplicate declaration that panicked the compiler. Generated programs are safe to execute *by construction*: `GenOptions::terminating` bounds every loop and makes the call graph acyclic (a function becomes callable only after its own body is emitted), because neither backend caps call depth and a stack overflow aborts the process rather than failing a test.
- **The check-vs-run project differential** (`noeta-fuzz`, `tests/project.rs`) — `noeta check` and `noeta run` do not share a front-end, and until this oracle only one of them was ever swept. `check` goes through `noeta_project::project_check` — the same entry the LSP's `workspace/diagnostic` and the MCP `check` tool use — which drives the **salsa** workspace (`noeta-db`); `run` goes through the loader's compile front-end. Every other target in `noeta-fuzz` drives the second one, which is why four defects had to be found by hand on the first: a re-derivation that never learned the loader's migrations/seeds exception (`check` reported E0074 on the filenames `noeta migrate new` generates), a linker gate narrower than the pass it guarded (`check` accepted a package `run` refused), a `file:` URI where a path belonged, and a program-wide import table letting one file's `use` capture bindings in every other. Three of those are literally "check says X, run says Y", and none was reachable from a generated *program*: each is a fact about the project's **layout**, so the input here is a package on disk — a manifest, `src/`, a subdirectory, a `migrations/` data directory, and file names that are and are not spellable as namespace segments, nine layouts cycled so every seam is covered every nine projects. The invariant is one boolean in both directions: `project_check` accepts a project **iff** the run-side front-end accepts every entry in it, over the same entry set (`project_check` checks every `.noe` file as its own entry, so the run side is asked about each of them too). Deliberately *not* asserted: diagnostic-set equality, which the two sides are not supposed to have — `check` deduplicates one module's fault across every entry that links it and folds several code-tier shapes together, while the run side reports per entry in one shape; the compile-to-bytecode step, which is already the execution oracle's invariant 1; and execution, which is what that oracle sweeps. Bodies come from the two existing generators rather than a third: the type-directed one (correct by construction) supplies the accepted half, where a false positive shows, and the syntax generator supplies the refused half, where a leniency divergence does. Its first 36-project probe found a live divergence — `noeta check .` exits 2 with "cannot read" on a package whose only `.noe` files live in `migrations/`, because the package walk prunes data directories and leaves the member set empty, while `noeta run` on that same file exits 0. That one is carried as a single argued exception (`is_open_divergence`) whose predicate is "the check said it could not read a file that is right there", asserted from *below* so it cannot outlive the bug it excuses.
- **The runtime-rejection census** (`noeta-fuzz`, `census.txt`, `tests/census.rs`) — the counterpart to fuzzing, and where fuzzing runs out. An oracle only finds what a generated program happens to reach, and "it stopped finding things" is not "there is nothing left". The runtime's static-class rejections are a *finite, enumerable* set — 186 sites across the two backends, 91 distinct reasons — and each is a question with a yes/no answer: can a program the checker accepts reach this? Working through them by hand produced nine more checker defects that no generated program had reached: conditions, literal patterns, callability, module functions, index types, iterability, `assert`, scalar exhaustiveness, enum-variant arity. The inventory is re-derived from the runtime's own source at test time and held against the checked-in snapshot, so a new rejection fails the build until someone records it — and recording it means answering the question. It is an inventory, not a verdict list: ~40 reasons are verified, the rest are recorded and unreviewed, and nothing asserts otherwise.
- **The leak oracle over generated programs** (`tests/leaks.rs`) — every other property here compares an *answer*, and a leak produces no wrong answer at all: the program prints what it should and exits zero, and the differential is happy. Both backends' per-thread live-object counters are sampled around each run, plus the VM's refcount-anomaly count (a missed retain/release that teardown's backup sweep absorbs, invisible to residency). The corpus version is the gate for programs somebody wrote; this one varies the object graphs.
- **The tier-1 differential over generated programs** (`tests/jit.rs`, `--features jit`) — the interpreter-vs-interpreter oracle cannot see anything that exists only in native code. Tier-1 compiles a *subset* of the language, so the interesting generated inputs are the ones that straddle the boundary and bail mid-frame.
- **What makes a runtime type error evidence.** The runtime constructs `TypeMismatch` at 159 sites and most are legitimately dynamic — a `dyn` member access *is* typed at run time. So the oracle restricts the input language rather than guessing: the generator emits no `dyn` and no reflection, and a test asserts that over the whole sweep. Value-dependent failures that share a static code (`[1, 2] - [3]` is a length mismatch, not a type one) are an explicit, argued exception list. Both are the same discipline as the parse-rate floor, applied to classification instead of coverage.
- **Triage is part of the tool, not an afterthought.** A fuzzer that only reports "seed 92 fails" leaves the real work undone. `triage` reproduces a seed, groups failures into families, and delta-debugs a case down by lines — a reduction that breaks the parse makes the component decline it, which reads as "no longer fails" and is rejected, so the minimizer stays syntactically valid without encoding any grammar. Reductions of 100+ lines to under 10 are routine; the printer bugs above all reduced to one-liners.
- **The leak oracle** — heap residency must be 0 at clean exit, both backends, whole corpus.
- **The refcount-anomaly oracle** — during cycle collection, every unreachable object's refcount must equal its in-edges from the garbage set (unreachable garbage can only reference itself), so a *skipped* retain or release is caught even when teardown's backup sweep would have absorbed the orphan. Runs inside the leak and JIT oracles.
- **The JIT oracle** (`--jit-differential`, `jit` feature) — every corpus program through the interpreter and the forced-Tier-1 JIT: byte-identical `RunResult`, zero residency, zero anomalies. This is the gate for native-code refcount contracts that miri cannot see.
- **The AOT oracles** (row 9 of the parallel-path audit) — `noeta build --native` links a *second shape* of native code (inline caches off, null call sites, no cancellation poll) into a binary with no compiler in it, and until 2026-08 the only thing gating it was one hand-written program comparing stdout. Two arms now: `--jit-differential --aot-bodies` runs the whole corpus through that body shape in-process (per-commit, no linker), and `--aot-differential` builds the real artifact per corpus program and compares it against `noeta run` over the same module — stdout, stderr and exit code (gate tier, one `cc` link per program). Both are in `scripts/gate.sh` and `ci.yml`.
- **miri** — covers the quarantined `unsafe` that can execute under it: `noeta-value`, `noeta-gc`, the `noeta-db` newtype, a `noeta-stdlib` reinterpret, and the test-only `noeta-alloc-probe`. The compiler and the default-feature interpreter paths are `unsafe`-free.
- **What miri cannot see — the JIT oracle covers it.** The Tier-1 seam's `unsafe` (`noeta-jit`, the VM's `jit`-feature helpers) executes as generated native code, which miri can't run; the JIT oracle above is that code's gate for memory and refcount correctness.
- **The hot-reload end-to-end suites** (`scripts/hot-e2e.sh`) — `hot_serve`, `hot_live`, `parallel_hot`, `live_serve`, `graceful_drain`, driven through the shipped binary against a real listening socket: edit a running handler and assert the new body serves, the signal state survives, the swap reaches every worker, and a live client is told to reload. They stay `#[ignore]`d because they bind ports and spawn processes, so nothing runs them implicitly; the script is the one place that does, called by both the `jit` CI job and the merge tier of `scripts/gate.sh`. Everything else about a hot swap is checked on the compile side — this is the only gate that watches one land in a server that is actually serving.
- **Coverage** — measured with `cargo-llvm-cov` (never tarpaulin, which can't see the subprocess-driven CLI tests). `cargo llvm-cov --workspace --summary-only`.
- **Benchmarks** — `cargo bench -p noeta-vm` runs the `criterion` benches over the VM hot paths; a VM-touching change should check for no regression.

## Before you're done

`scripts/gate.sh` runs the CI workflow's jobs locally, in the same split, and prints a per-step PASS/FAIL summary — a step that cannot run is reported SKIP, never PASS, and a failure never stops the remaining steps. Run the tier that matches the moment:

```sh
scripts/gate.sh --quick   # fmt + both clippy splits                        (1m20s warm)
scripts/gate.sh           # + the suite & oracles, doc samples, JIT gates,
                          #   and the real-socket hot-reload e2e suites     (~15 min warm, 35 cold)
scripts/gate.sh --full    # + wasm portability, the linked --native AOT
                          #   differential, miri, editor tooling            (before a release tag)
```

- Zero compiler warnings (`cargo build` produces no `warning:` lines).
- The gate is green at the merge tier, and `--differential` is at 0 skipped.
- New functionality has tests; architectural changes update the docs.

## Conventions (summary — full list in `AGENTS.md`)

- Conventional-commit titles; work on a branch/worktree to avoid conflicts with parallel agents.
- American English in code, comments, and docs.
- No hard line-wrap in Markdown.
- Prefer enums/constants over magic strings; keep `unsafe` quarantined and justified.
- Each crate keeps its `README.md` current; keep `README.md`/`AGENTS.md`/`ARCHITECTURE.md`/these docs aligned when architecture or features change.

## The docs themselves

The docs live in the repo under `docs/` — versioned together with the code — as flat pages with `_Sidebar.md` for navigation, published to docs.noeta.dev. When you change a feature, update the relevant reference page here alongside the code — the same "lands with its docs" discipline as the conformance rule.
