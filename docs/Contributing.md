# Contributing & Developer Guide

This page is the developer's entry point to *building* the language, meaning the compiler rather than programs written in it. It orients you and points at the in-repo `AGENTS.md` (the exhaustive conventions reference) and `ARCHITECTURE.md` (the technical overview).

Noeta is a pre-1.0 implementation, public and with tagged releases, built primarily through agentic engineering. The discipline below is written to be followed by humans and AI agents alike.

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

You need a recent stable Rust (1.95+). CI pins its toolchain to 1.97.0, and clippy's lint set is version-sensitive, so lint with `cargo +1.97.0 clippy` locally: a floating `stable` adds lints that surface only as a red CI you did not cause.

### What a conformance run proves

The two conformance commands answer **different** questions, and each prints which one it answered:

- **The expectation run** (no oracle flag) checks every case against its `// expect:` header, executing the program on the reference interpreter *and* the bytecode VM. Both, because a header checked against one engine is a claim about half the implementation, and a regression living in a single backend would pass. Every failure names the engine that produced it, and the summary counts what each engine ran.
- **The differential** (`--differential`) compares the two backends' full output against *each other* on every program, whatever the headers say. It is the oracle for behavior no header pins down.

Two flags narrow the expectation run. `--engine reference` and `--engine vm` localize a failure you already have. `--stage lexer` and `--stage parser` stop before execution, asserting the `// expect: error` lines and leaving stdout, stderr and the exit code unchecked, which the summary says out loud.

A `--file` that matches no case exits 2 rather than reporting an empty run as a pass. The narrowed run is what gets cited as evidence that a fix works, so it has to be a statement about something.

## The compilation pipeline

Source flows through single-purpose crates: lexer → parser → checker → IR → RC passes → two backends. Each stage is a separate crate with explicit input and output types and no hidden shared mutable state, so a change stays local to one crate and is verifiable by that crate's snapshots. The full diagram and crate map are on [Architecture & Pipeline](Architecture-and-Pipeline). Every crate carries a `README.md` as its primary documentation, which is where to start when working in one.

## The differential oracle

The language is implemented twice, as a reference interpreter (`noeta-eval`) and a bytecode VM (`noeta-vm`), and the conformance harness asserts their observable output is byte-for-byte identical on every corpus program, with a hard **`0 skipped`** gate. That is the backbone of the test strategy, and two rules follow:

1. **A feature lands in both backends**, or is explicitly oracle-exempt. Shared semantics go in `noeta-stdlib` so the two agree by construction rather than by luck.
2. **The differential stays green** after every change: `--differential` at 0 skipped, backends agreeing.

Agreement covers half the claim. Two backends agree perfectly on a wrong answer whenever the fault is in what they share, which is what the `// expect:` headers pin, and why the expectation run drives both engines rather than the reference alone. The differential says the halves match; the headers say the language does what it says.

## The new-feature template

A language feature is added as a **vertical slice**, in this order:

1. **Grammar / AST** — token(s) in `noeta-lexer`, node(s) in `noeta-ast`, production in `noeta-parser` (keep surface sugar as its own AST node).
2. **Checker rule** — typing/inference in `noeta-check` (plus a new `Type` form in `noeta-types` if needed); add a negative conformance case for any new static-error class.
3. **Lowering** — AST → IR in `noeta-ir` (plus `noeta-ir-passes` if the feature introduces owned heap values needing drops/reuse).
4. **Both backends** — evaluation in `noeta-eval` **and** bytecode in `noeta-compiler`/`noeta-vm`; keep shared semantics in `noeta-stdlib`.
5. **Conformance cases** — `tests/conformance/**.noe` with `// expect:` headers, error cases included; must run `--differential` at 0 skipped.
6. **Snapshot update** — review `insta` snapshots deliberately; never blind-accept.

> **The iron rule: every feature or fix lands with a conformance corpus entry.** Prefer vertical-slice tasks ("implement `~` end to end") over diffuse refactors, since a slice's done-condition is "its conformance cases pass."

## Testing architecture

Each row below is a separate claim about the implementation, with the command that makes it. A row that a change touches should be run before the change is committed, and `scripts/gate.sh` runs the whole set in CI's own split.

| Oracle | What it proves | How to run it |
|---|---|---|
| **Per-stage snapshots** (`insta`) | tokens, AST and rendered diagnostics match their pinned form at each stage boundary | `cargo test` |
| **Conformance corpus** (`tests/conformance/**.noe`) | every case's *stated* `// expect:` output, on the reference interpreter and the VM alike | `cargo run -p noeta-conformance` |
| **The differential** | the two backends' whole observable result is identical, over every corpus program | `cargo run -p noeta-conformance -- --differential` |
| **Property tests** (`proptest`) | invariants such as parse→print→parse round-trips, and the static-≤-dynamic last-use property for the RC passes | `cargo test` |
| **The formatter fuzz oracle** (`noeta-fuzz`, `tests/fmt.rs`) | `noeta fmt` over generated programs, across the layout and config space a corpus barely samples | `cargo test -p noeta-fuzz --test fmt` |
| **The generator's own gate** (`tests/generator.rs`) | the generator's parse rate, its totality over degenerate buffers, and the determinism that reproduction from a reported seed depends on | `cargo test -p noeta-fuzz --test generator` |
| **The execution oracle** (`tests/run.rs`) | a checked program compiles, does not fail *statically* at run time, and produces the same result on both backends | `cargo test -p noeta-fuzz --test run` |
| **The check-vs-run project differential** (`tests/project.rs`) | `project_check` accepts a generated project **iff** the run-side front-end accepts every entry in it | `cargo test -p noeta-fuzz --test project` |
| **The runtime-rejection census** (`census.txt`, `tests/census.rs`) | the runtime's static-class rejection reasons, re-derived from its own source and held against the checked-in inventory | `cargo test -p noeta-fuzz --test census` |
| **The leak oracle** | heap residency is 0 at clean exit, on both backends, over the whole corpus | `cargo run -p noeta-conformance -- --check-leaks` |
| **The leak oracle over generated programs** (`tests/leaks.rs`) | the same residency claim over object graphs the corpus does not build, plus the VM's refcount-anomaly count | `cargo test -p noeta-fuzz --test leaks` |
| **The refcount-anomaly oracle** | during cycle collection, every unreachable object's refcount equals its in-edges from the garbage set, so a *skipped* retain or release is caught even when teardown's backup sweep would have absorbed the orphan | a VM-side measurement, running inside the leak and JIT oracles |
| **The skipped-destructor oracle** | every object allocated with a destructor-bearing shape ran its `destruct`, which residency alone cannot see and both backends agree on when the drop is missing from the shared IR | a VM-side measurement, running inside the leak and JIT oracles, corpus only |
| **The JIT oracle** | every corpus program is byte-identical between the interpreter and the forced Tier-1 JIT, at zero residency and zero anomalies | `cargo run -p noeta-conformance --features jit -- --jit-differential` |
| **The cancellable-codegen arm** | the same byte-identity with the JIT's loop-header cancellation poll emitted on every compiled body | add `--cancel-poll` |
| **The AOT body oracle** | the shape `noeta build --native` links (inline caches off, null call sites, no cancellation poll) gives the same `RunResult`, in-process and with no linker | add `--aot-bodies` |
| **The linked AOT differential** | the real artifact's stdout, stderr and exit code match `noeta run` over the same module, one `cc` link per corpus program | `cargo run -p noeta-conformance -- --aot-differential`, with a C toolchain |
| **The wasm differential** | every corpus program compiled to a `.noeb` and run under wasmtime matches the native VM's stdout, exit code and rendered stderr | `cargo run -p noeta-conformance -- --wasm-differential`, with `wasmtime` and a built `noeta-wasm-runner` |
| **The tier-1 differential over generated programs** (`tests/jit.rs`) | generated programs that straddle the JIT's compiled subset agree with the interpreter | `cargo test -p noeta-fuzz --test jit --features jit` |
| **The real-host corpus gate** | every async corpus case, run through the shipped `noeta` binary on a real host, holds to its own `// expect:` header | `cargo test -p noeta-cli --test conformance_real_host` |
| **miri** | the `unsafe` in `noeta-value` and `noeta-gc` executes under the miri interpreter without undefined behavior | `cargo +nightly miri test -p noeta-value -p noeta-gc --locked` |
| **The hot-reload end-to-end suites** | a swap lands in a server that is actually serving | `scripts/hot-e2e.sh` |
| **The docs oracles** | every ` ```noeta ` block runs through the real binary, and every ` ```toml ` block parses as a manifest the toolchain accepts | `cargo test -p noeta-cli --test doc_samples` |
| **Coverage** | which lines a change left unexercised | `cargo llvm-cov --workspace --summary-only` |
| **Benchmarks** | the VM hot paths (dispatch loop, inline-cached property access, allocation) | `cargo bench -p noeta-vm` |

Coverage is measured with `cargo-llvm-cov` rather than tarpaulin, which cannot see across a process boundary and reports the subprocess-driven CLI tests as 0% coverage of the `noeta` binary. Treat a coverage drop on a file you touched as a regression.

### What structured fuzzing adds

`noeta-fuzz` turns a `&[u8]` into a *syntactically valid* Noeta program, so the properties under test are evaluated past the parser. Byte-level mutation is near-useless against a language front-end: almost every mutated buffer fails to lex, the component declines it, and every property passes vacuously.

Generation is total. Bytes are consumed left to right, and once they run out every further choice reads as `0`, with each choice list ordered so `0` is the most terminal alternative. Exhaustion therefore winds a program down instead of truncating it mid-token, and any buffer yields something parseable. That contract is what lets one generator serve both a seeded deterministic sweep and a `proptest` driver that shrinks the *driver bytes* into a paste-ready reproducer.

What it varies is what a corpus of any size barely samples. Every corpus file has one layout, the one its author wrote and `noeta fmt` already normalized; the generator varies nesting, collapsed versus exploded block bodies, blank-line runs, semicolon presence, header parentheses, method-chain breaks, comment placement at every depth, and the whole config space rather than the three configurations the corpus pins.

Generated programs are safe to execute by construction. `GenOptions::terminating` bounds every loop and makes the call graph acyclic, a function becoming callable only after its own body is emitted, because neither backend caps call depth and a stack overflow aborts the process rather than failing a test.

### Keeping a structured fuzzer honest

A structured fuzzer fails silently by drifting into unparseable text, leaving the suite green while it tests nothing. So the generator's parse rate is asserted rather than reported (`tests/generator.rs`), alongside totality over degenerate buffers and determinism.

Every oracle carries a *reach* assertion for the same reason. The formatter's is the parse rate, the `.noeb` container's is how many corrupted bundles get past the header, the execution oracle's is how many generated programs actually run (~8%, the rest being rejected by the checker, legitimately), and the project differential's is how many generated projects *each* front-end accepted.

That last one is floored on both sides, because its invariant is a boolean and a population that only ever takes one value proves one implication and skips the other. Without a reach assertion, a sweep that tests nothing is indistinguishable from a sweep that finds nothing.

Any bounded sweep also states its bound. `SEEDS` in `tests/fmt.rs` and `NONCES` in `tests/run.rs` and `tests/project.rs` are floors rather than claims, and the deep sweeps run the identical oracles as far as you care to wait:

```sh
cargo run --release -p noeta-fuzz --example triage      -- scan 50000
cargo run --release -p noeta-fuzz --example runscan     -- scan 120000
cargo run --release -p noeta-fuzz --example projectscan -- scan 20000
```

### What makes a runtime type error evidence

The runtime constructs `TypeMismatch` at 159 sites, and most are legitimately dynamic, since a `dyn` member access *is* typed at run time. The oracle restricts the input language rather than guessing: the generator emits no `dyn` and no reflection, and a test asserts that over the whole sweep. Value-dependent failures that share a static code (`[1, 2] - [3]` is a length mismatch rather than a type one) are an explicit, argued exception list. Both are the discipline behind the parse-rate floor, applied to classification instead of coverage.

### Triage

`triage` is part of the tool. It reproduces a seed, groups failures into families, and delta-debugs a case down by lines. A reduction that breaks the parse makes the component decline it, which reads as "no longer fails" and is rejected, so the minimizer stays syntactically valid without encoding any grammar. Reductions of 100 lines and more down to under 10 are routine.

### Where fuzzing runs out

The runtime-rejection census is the counterpart to fuzzing. An oracle finds only what a generated program happens to reach, and "it stopped finding things" is a different statement from "there is nothing left".

The runtime's static-class rejections are a *finite, enumerable* set, 186 sites across the two backends and roughly 90 distinct reasons, and each is a question with a yes/no answer: can a program the checker accepts reach this? The inventory is re-derived from the runtime's own source at test time and held against the checked-in `census.txt`, so a new rejection fails the build until someone records it, and recording it means answering the question. It is an inventory rather than a verdict list: roughly 40 reasons are verified, the rest are recorded and unreviewed, and nothing asserts otherwise.

### What the project differential covers

`noeta check` and `noeta run` do not share a front-end. `check` goes through `noeta_project::project_check`, the same entry the LSP's `workspace/diagnostic` and the MCP `check` tool use, which drives the **salsa** workspace (`noeta-db`); `run` goes through the loader's compile front-end. Every other target in `noeta-fuzz` drives the second one.

The defects this oracle covers are facts about a project's **layout** rather than about a program, so its input is a package on disk: a manifest, `src/`, a subdirectory, a `migrations/` data directory, and file names that are and are not spellable as namespace segments. Nine layouts are cycled, so every seam is covered every nine projects.

The invariant is one boolean in both directions. `project_check` accepts a project **iff** the run-side front-end accepts every entry in it, over the same entry set, since `project_check` checks every `.noe` file as its own entry and the run side is asked about each of them too.

Three things are deliberately *not* asserted: diagnostic-set equality, which the two sides are not supposed to have, because `check` deduplicates one module's fault across every entry that links it and folds several code-tier shapes together while the run side reports per entry in one shape; the compile-to-bytecode step, which is already the execution oracle's first invariant; and execution, which is what that oracle sweeps.

Bodies come from the two existing generators rather than a third. The type-directed one is correct by construction and supplies the accepted half, where a false positive shows; the syntax generator supplies the refused half, where a leniency divergence does.

### What miri cannot see

The Tier-1 seam's `unsafe` (`noeta-jit`, and the VM's `jit`-feature helpers) executes as generated native code, which miri cannot run. The JIT oracle is that code's gate for memory and refcount correctness. The compiler and every default-feature interpreter path stay `unsafe`-free.

### Real time and logical time in the corpus

Every corpus oracle except two runs on `SandboxHost` and `SandboxExecutor`, where `advance` *jumps* the clock to exactly the next deadline, so `sleep(1)` and `sleep(2)` can never come due at the same poll. The AOT differential is on a real host and compares its two real runs to each other rather than to the header. The real-host corpus gate sits between them, holding a real run to the header it was written for.

On a real host the executor sleeps real time to the earliest deadline and wakes late, every deadline the overshoot crossed comes due at the same poll, and the scheduler resumes those tasks in spawn order. A corpus case therefore states its timing in gaps that survive a loaded scheduler. The cases that genuinely cannot, needing a real OS thread or a capability the sandbox host stubs, are listed with a reason that the gate prints on every run.

### The hot-reload suites

`scripts/hot-e2e.sh` drives `hot_serve`, `hot_live`, `parallel_hot`, `live_serve` and `graceful_drain` through the shipped binary against a real listening socket. Each edits a running handler and asserts that the new body serves, the signal state survives, the swap reaches every worker, and a live client is told to reload.

They stay `#[ignore]`d because they bind ports and spawn processes, so nothing runs them implicitly. The script is the one place that does, called by both the `jit` CI job and the merge tier of `scripts/gate.sh`. Everything else about a hot swap is checked on the compile side, and this is the gate that watches one land in a server that is serving.

### The docs oracles

There are two, because a page has two halves a reader copies. Every ` ```noeta ` block runs through the real binary: untagged must exit 0, `check` must type-check, `error` must fail, and `ignore` opts out. Every ` ```toml ` block must parse as a manifest the toolchain accepts.

The manifest half exists because the parser is strict enough to be worth asking. It refuses a table the schema does not define, and a directive provider that names no dependency, which is how a page came to declare `crit = "acme/criterion:bench"` two lines under its own sentence saying that is an error. `[package]` is optional, so a fragment such as `[directives]` alone is checked as written. Tag a block ` ```toml ignore ` when it is deliberately not a `noeta.toml`: a `Cargo.toml`, a Spin manifest, a `noeta.lock` excerpt.

## Before you're done

`scripts/gate.sh` runs the CI workflow's jobs locally, in the same split, and prints a per-step PASS/FAIL summary. A step that cannot run is reported SKIP, never PASS, and a failure never stops the remaining steps. Run the tier that matches the moment:

```sh
scripts/gate.sh --quick   # fmt + both clippy splits                          (a minute)
scripts/gate.sh           # + the suite & oracles, lean-CLI and feature shapes,
                          #   doc samples, JIT gates, the real-socket hot-reload
                          #   e2e suites, and the perf ratchet     (tens of minutes)
scripts/gate.sh --full    # + wasm portability/differential, the linked --native
                          #   AOT differential, miri, editor tooling  (before a release tag)
```

Read the command's exit code rather than a pipe's. `cargo clippy … | tail` reports `tail`'s status, so a broken tree passes a local check; redirect the output to a file and read the file.

- Zero compiler warnings (`cargo build` produces no `warning:` lines).
- The gate is green at the merge tier, and `--differential` is at 0 skipped.
- New functionality has tests; architectural changes update the docs.

## Conventions (summary — full list in `AGENTS.md`)

- Conventional-commit titles; work on a branch and an isolated worktree, since parallel agents share this checkout.
- American English in code, comments, and docs.
- No hard line-wrap in Markdown.
- Prefer enums and constants over magic strings; keep `unsafe` quarantined and justified.
- Each crate keeps its `README.md` current; keep `README.md`, `AGENTS.md`, `ARCHITECTURE.md` and these docs aligned when architecture or features change.

## The docs themselves

The docs live in the repo under `docs/`, versioned together with the code, as flat pages with `_Sidebar.md` for navigation, published to docs.noeta.dev. When you change a feature, update the relevant reference page here alongside the code, under the same "lands with its docs" discipline as the conformance rule.
