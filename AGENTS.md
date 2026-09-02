# AGENTS.md

**Noeta** is a new programming language built from scratch in Rust: a bytecode VM + Cranelift JIT, an inferred-static type checker, precise reference counting over an ANF IR, and a toolchain (CLI, LSP, DAP, MCP, formatter, profiler, package manager) that ships as a single binary. This file is the conventions + workflow contract for agents working on the *implementation*. Read it before editing anything.

## Where to look first

- `ARCHITECTURE.md` — the pipeline diagram, the crate map, and the key implementation decisions. **The crate map lives there and only there**; do not restate it here.
- `docs/` — the wiki (flat pages, `_Sidebar.md` navigation, published to docs.noeta.dev). `docs/Home.md` is the entry; `docs/Contributing.md` is the developer guide and the long form of the testing strategy.
- `plans/roadmap.md` (the frontier) and `plans/backlog.md` (every open item). Completed arc ledgers live in git history, not in `plans/`.
- Each crate's own `README.md` is that crate's primary documentation — start there when working inside one.

## The new-feature template

A language feature lands as a **vertical slice**, in this order: token/AST node (`noeta-lexer`, `noeta-ast`, `noeta-parser`, keeping surface sugar as its own AST node) → checker rule (`noeta-check`) → lowering (`noeta-ir`, plus `noeta-ir-passes` if it introduces owned heap values) → **both backends** (`noeta-eval` the oracle *and* `noeta-compiler`/`noeta-vm`, with shared semantics in `noeta-stdlib` so they agree by construction) → conformance cases → reviewed `insta` snapshots. `docs/Contributing.md` has the annotated version.

> [!IMPORTANT]
> **The iron rule: every feature or fix lands with a conformance corpus entry** — a `tests/conformance/**.noe` file with an `// expect:` header, negative cases included, and `cargo run -p noeta-conformance -- --differential` at **0 skipped**. Prefer vertical-slice tasks ("implement `~` end to end") over diffuse refactors: a slice's done-condition is "its conformance cases pass". Never blind-accept an `insta` snapshot.

> [!IMPORTANT]
> **A test you have not seen fail is not a test — ablate it, and commit before you do.** Reintroduce the defect, watch the new case go red *for the reason it names*, then restore. The corpus is full of cases that passed because they never reached the thing they claimed to check: one asserted "histograms and gauges are capped the same way" while recording 100 attribute sets under a limit of 2000, so it never folded a histogram at all; another's own comment cited the differential oracle that had been skipping it. A case whose program cannot reach its claim reads exactly like one that holds.
>
> **Commit first.** Ablation means editing the source and putting it back, and `git checkout -- <file>` reverts to HEAD — so uncommitted *tests* in that file vanish with the ablation, and the run that follows passes against a file no longer containing them. That has happened twice in one session, once caught only by noticing a test count of 10 where 13 had run a minute earlier. Commit the test, then ablate, then restore.
>
> Two ablation results are not verdicts: a guard you cannot make fail without building a feature that does not exist yet is a **forward-guard** — say so rather than counting it as verified. And a negative case can go red for the wrong reason: a runtime abort and a static rejection can raise the same code at the same span, so pin something only one of them produces.

## Code conventions

- Files `snake_case.rs`, types `PascalCase`, functions/variables `snake_case`, constants `SCREAMING_SNAKE_CASE`.
- **American English** everywhere — code, comments, docs: `behavior` not `behaviour`, `serialize` not `serialise`.
- **Prefer enums and constants over magic strings.** Variant names, format identifiers, provider names and severity levels are enums with `Display`/`FromStr` (or `strum` derives), never ad-hoc string comparisons — an exhaustive `match` is what stops one backend silently omitting a case.
- Performance-oriented architecture, SOLID, DRY. Avoid god-objects; prefer dependency injection and the strategy pattern. Take DDD inspiration where it applies (not in data-oriented code).
- New `unsafe` needs a justification and an entry in the `ARCHITECTURE.md` quarantine list — the workspace is `unsafe_code = "forbid"` with per-crate opt-outs.

## Formatting & linting

- `cargo fmt --all` before every commit. There is no `rustfmt.toml` — default rustfmt style, deliberately.
- `cargo clippy --all-targets --locked -- -D warnings`, in both splits (default features, and `--features "noeta-vm/jit noeta-conformance/jit"` for `noeta-jit`/`noeta-cli`). Fix diagnostics; do not `#[allow]` without a written reason.
- **The CI toolchain is pinned to `1.97.0`** (`dtolnay/rust-toolchain@1.97.0` in every `ci.yml`/`release.yml` job except miri, which is nightly). Clippy's lint set is version-sensitive, so lint with `+1.97.0` locally — a floating `stable` adds lints that surface only as a red CI you did not cause. `scripts/gate.sh` selects the pin for you when it is installed.

> [!IMPORTANT]
> **Read the command's exit code, never a pipe's.** `cargo clippy … | tail` reports `tail`'s status, so a broken tree passes a local "check". That is not hypothetical: a `clippy -D warnings` violation reached `main` under a local run that *reported a pass* for exactly this reason. To read the output, redirect it to a file and read the file.

## Testing

This project is developed primarily by coding agents, so the test suite is the thing keeping quality honest. `docs/Contributing.md` documents every oracle (differential, leak, refcount-anomaly, JIT, AOT, wasm, fuzz, census, hot-reload). Three things that are easy to get wrong:

**Coverage** is measured with `cargo-llvm-cov`, **never** `cargo-tarpaulin` — tarpaulin cannot see across a process boundary, so it reports the subprocess-driven CLI tests in `crates/noeta-cli/tests/` as 0% coverage of the `noeta` binary. `cargo llvm-cov --workspace --summary-only`, or `--html` for a browsable report; set up with `rustup component add llvm-tools-preview && cargo install cargo-llvm-cov`. Treat a coverage drop on a file you touched as a regression to fix.

> [!IMPORTANT]
> **A language change silently rots hand-written fixtures that have no expected-output file.** The conformance corpus is protected (every case pins its `// expect:` output); Rust in-test fixtures — `.noe` source embedded in `#[test]`s — are not. They just stop compiling under the new rule with nothing asserting they still should. After any language change, grep for them and re-run. `examples/` used to have the same hole and no longer does: `crates/noeta-cli/tests/examples.rs` drives every shipped example through the real `noeta` binary, because two language rules each broke a LiveView example and both sat broken in the tree.

> [!IMPORTANT]
> **Performance regressions are gated on INSTRUCTIONS RETIRED, not wall-clock** — `scripts/perf-ratchet.sh`, run by `scripts/gate.sh` at the merge tier. Wall-clock cannot gate on this box: several agent builds run concurrently, load 6–13 is normal, and a whole field of wall-clock benchmarks inflates ~2× together, so a 7% regression is indistinguishable from a busy afternoon. Roughly 1,800 commits once landed a **2× startup regression and a 7–11% interpreter regression** and nothing in the tree noticed. Instructions retired repeat to 0.001–0.08% under exactly that load, so the ratchet pins five rows against `tests/perf/baseline.txt` and asserts **which engine each row ran on** — equal JIT and interpreter numbers mean the JIT never ran, which is a finding, not a footnote.
>
> Expect to re-baseline: perf work lands continuously and an improvement past the band FAILS on purpose, so the ratchet holds the new floor. `scripts/perf-ratchet.sh --record` rewrites the baseline; **read the diff's sign before committing it** — down is an improvement worth keeping, up needs a sentence naming the change that bought it and what for. Build with the gate's pinned toolchain first (`cargo +1.97.0 build --release -p noeta-cli`): the baseline records cpu, libc *and* `rustc -V`, because two rustc patch releases inline differently and here that is worth percent. A machine or compiler mismatch reports **CANNOT MEASURE (exit 2)** — never a pass, never an invented regression. The measured variance behind each tolerance is documented at the top of the script.

`cargo bench -p noeta-vm` runs the `criterion` benches over the VM hot paths (dispatch loop, inline-cached property access, allocation) in `crates/noeta-vm/benches/vm.rs`; a VM-touching change should check them.

## Writing docs

`docs/` is the wiki, and it describes what Noeta **is** — for a reader using the toolchain today, who never saw any earlier version. Three rules follow, each of which has had to be repaired at least once:

- **No history.** Not "this used to", "was previously", "the old advice", a before/after measurement, a retracted recommendation, or a note that a setting moved from one table to another. The reader never knew the old behavior, and a changelog sentence inside a reference page reads as a live rule. State the current rule; git carries the rest.
- **No milestone vocabulary.** Arc, slice and phase labels (`P-AOT L1`, `P-WASM W4`, `object-model slice 6`, `(interim)`) mean nothing outside this repo. **This covers `--help` text and diagnostics, not just `docs/`** — a clap `///` doc comment is user-facing, and four `noeta build` flags shipped announcing their internal milestone.
- **No links into `plans/`.** The wiki must not send a reader to a roadmap document, or to git history for a design record. (`docs/Contributing.md` is the one exception: pointing a *contributor* at the roadmap is its job.)

Rationale is welcome where it helps a reader *use* the thing — explaining that acronyms are words because `HTTPSURLParser` has no readable boundaries is what makes the rule stick. It is not welcome as a defense of a decision against alternatives nobody proposed.

### How a page reads

Write what a thing **is** and how to use it. A reader arrives with a task, so the page owes them the shape of the answer and a sample they can copy, before it owes them anything else.

The patterns below are banned because each one costs a reader time without paying it back. Most of them are ways of sounding considered rather than being clear.

**Say what it is, not what it isn't.** "A union orders when every member orders" beats "ordering is not defined unless the members order". Negative definition makes a reader reconstruct the positive case themselves. This extends to the "not X, but Y" frame, to arguing against a design nobody proposed, and to trailing justification clauses that restate the rule as a moral.

**Do not announce a point before making it.** A paragraph that opens by declaring its own thesis, or a sentence that promises candour before delivering content, wastes the reader's first line. Start with the content.

**Keep the prose plain.** No em dashes. No colon-then-payload for emphasis. No punchy fragments. No sentence under six words carrying a whole thought. No triads or four-item lists where two items or a table would do. No paragraph whose sentences all land at the same length. No metaphor verb standing in for description: a flag does not *cut through* anything, and a number does not *move the needle*.

**Drop the intensifiers.** `genuinely`, `quietly`, `only ever`, `nobody talks about`. They add emphasis a technical page cannot cash.

**One idea per paragraph, and stop.** A paragraph that restates its predecessor in other words should be deleted, not rephrased. Paired antithetical clauses ("keep what works, drop what doesn't") and mirrored "which … and which" contrasts are two sentences pretending to be one.

**Claim only what you can point at.** A page must not assert what a reader's codebase looks like or where their project is heading.

Reference pages want tables. A method, its signature and one example belong in a row, and the prose around them should shrink to the rules a table cannot hold.

`cargo test -p noeta-cli --test docs_style` enforces the mechanical half (the phrasings and labels above, in `docs/` and in the CLI's user-facing help). It is a lint over a small set of high-precision patterns, not a proof — the judgment half is yours, and a wiki page written as a changelog will pass it.

> [!NOTE]
> **Markdown in this repo never uses hard line wrap** — one line per paragraph.

## Working alongside other agents

> [!IMPORTANT]
> **ALWAYS create a branch AND an isolated git worktree before editing any file — never work in the shared root checkout `/home/niklas/Code/lang`.** Many agent sessions run against this same repo concurrently. The root floats between branches and is actively edited: a parallel session's `git add -A && git commit` will sweep your uncommitted files into *its* commit, and a stray `git reset --hard`/`git restore` destroys *their* live work.
>
> Start every task with `git worktree add -b <branch> .claude/worktrees/<name> main`, then work, build and commit there. If work has already leaked into the root, recover non-destructively: `git reset` (unstage only, never `--hard`), branch a worktree from the polluting commit, finish there, and `git restore` only your *own* stray files out of the root.

> [!WARNING]
> **Six sharp edges, each of which has burned an agent for real:**
>
> - **The Bash working directory silently resets to the shared root** (typically after a command that `cd`s elsewhere; the "Shell cwd was reset to …" line is easy to miss). A relative-path edit then lands in the *root*, and `git`/`cargo` report the *root's* state — which has looked like "phantom compile errors" and "a lost HEAD" that were never real. **Use absolute paths for every file and for `CARGO_TARGET_DIR`, and confirm the directory (`cd <worktree> && pwd && git log --oneline -1`) before trusting any `git`/`cargo` output.** Prefer the Edit/Write tools (cwd-independent) over shell heredocs for source edits.
> - **`git stash` is shared per-repository across all worktrees** — never use it as a "clean tree" control. A pop can conflict with, or surface, another agent's stash, and it does not stash committed work anyway. To compare against a baseline, check the commit out in a separate throwaway worktree.
> - **A scratch directory outside the repo is shared far more widely than it looks** — agent scratch and task-output paths are keyed by *project directory*, not by session, so concurrent sessions on this repo share one directory, and `/tmp` is global. Two agents writing `suite.log` there truncate each other's, and the second to read it sees the *other's* result under its own name. That has already produced a wrong claim in a report, and it is worse for a *diagnosis*: a foreign red read as your own gets chased for an hour before anyone checks the binary path in the error text. **Put your branch name in any filename you write outside your worktree**, or write inside it. When a failure surprises you, read the paths in its output before believing it is yours.
> - **Ship from the *main* checkout, not the worktree.** `git merge <branch>` run from inside that branch's own worktree is a silent no-op ("Already up to date"), and `git worktree remove` then deletes your own cwd. Split the step: commit inside the worktree; `cd /home/niklas/Code/lang` as a *fresh* command; merge and `git worktree remove <path>` from there.
> - **Other sessions' worktrees under `.claude/worktrees/` may hold live, uncommitted work.** Before removing any worktree or deleting any branch, check `git status --porcelain` in it and confirm with the user — a branch whose tip is merged may still carry the next feature on top.
> - **Never `pkill`/`pgrep` by pattern, and never wait on a background job from the foreground.** Both halves cost a full gate run in one afternoon. A `pkill -f 'scripts/gate.sh'` matched *another session's* gate and killed it: the victim saw its run vanish mid-step with no diagnostic and no failing test, which is indistinguishable from an infrastructure failure in its own patch. Kill only PIDs you captured when you started the process, and confirm ownership through `/proc/<pid>/cwd` and `/proc/<pid>/environ` first. Separately, waiting on a backgrounded gate with a foreground `tail --pid` makes the gate a casualty of the *waiter's* termination — the run died with exit 144 when the foreground command was cut. Use the harness's background mechanism to wait, never a foreground tail or poll loop.

> [!IMPORTANT]
> **Use a per-agent `CARGO_TARGET_DIR`, and set `CARGO_BUILD_JOBS`.** A shared target dir causes last-writer-wins rlib contamination (identical-version path crates) and phantom test failures. Target dirs are ~70–210 GB *each* and `/home` (450 GB) fills fast — a full disk has **truncated a source file mid-write**. Check `df -h /home` before a long build, and **do not build in `/tmp`** (a 14 GB tmpfs; the build dies partway).
>
> **Three agents can build concurrently; three cannot *gate* concurrently.** Measured on one afternoon, three simultaneous merge-tier runs held 217 + 167 + 138 GB and took `/home` from 205 GB free to 12 — the tier's lean-CLI and feature shapes each add a fresh artifact set on top of the default one. Free space *oscillates* by tens of GB as builds allocate and release temporaries, so a single low reading is not a trend and is not grounds for killing anyone's run; sample it over minutes before acting. `target-agent/debug/incremental` alone reaches ~25 GB and is the cheapest thing to reclaim in **your own** dir when you need headroom mid-task.
>
> Isolating target dirs fixes correctness and makes throughput worse: cargo defaults to one job per core, so *each* concurrent build claims the whole machine — measured, three agents on a 20-core box drove load to **60** with 51 rustc processes, none blocked on a lock and every build running at a third speed. That reads exactly like a deadlock and is not one. Divide the cores by the number of agents and set it (`CARGO_BUILD_JOBS=6` for three agents on 20 cores). Before diagnosing a "stuck" build, check `nproc` against `uptime`'s load and the process states (`S<l` is running, not blocked): oversubscription and lock contention look identical from outside and have opposite fixes.
>
> **Two linker failures on this box are resource exhaustion wearing a compiler error's clothes, and both have been chased as code defects.** `ld terminated with signal 15` is the linker being killed under memory pressure — signal 15 is not a diagnostic, it is the OOM reaper, and it appears when several `-p noeta-cli` links land at once (seen at `CARGO_BUILD_JOBS=8`, load 26, 4 GB of swap in use; the same tree linked clean at 4). A linker `Bus error` is a full disk. Neither one is your patch. Check `df -h /home`, `free -g` and `uptime` before reading either as a real failure, and lower `CARGO_BUILD_JOBS` rather than bisecting your own change.

> [!IMPORTANT]
> **Reclaim your target dir the moment your work merges — prune branch, worktree and `CARGO_TARGET_DIR` together.** A finished-but-unpruned worktree is the single largest thing on this disk: two once held **267 GB**, with 201 GB more idle in a merged branch's cache. Left alone they took `/home` to **88% full**, where NVMe write throughput collapses and every concurrent build stalls on I/O rather than CPU (measured: load 112 with 0 runnable, 19 blocked, 60% iowait, 34% idle). Three separate "everything is slow" incidents in one day had three different causes, and only one was disk.
>
> **Order the prune: remove the worktree *first*, then the target dir — and re-check liveness immediately before deleting, not before the merge.** A merge and a prune are seconds apart for you and minutes apart for the agent still finishing its gate run. Reclaiming a target dir under a live `cargo test` corrupts that run (19 `noeta-cli` tests failed with `cannot read …/src/main.noe` — a result that looks exactly like a fixture-path bug and wasted a report explaining it) *and* leaves residue: the dying cargo keeps writing while `rm -rf` deletes around it. Removing the worktree first makes the cargo fail fast instead of racing.
>
> Two traps in deciding what is safe to reclaim: **`git merge-base --is-ancestor` compares hashes, so it reports rebased-and-landed work as unmerged** — two branches here looked like stranded fixes until `git cherry main <branch>` marked both `-` (already upstream); use `git cherry` or `git patch-id --stable` to ask "is this content on main?". And **`pgrep -f` will not find the owner of a target dir**, because cargo carries `CARGO_TARGET_DIR` in the *environment*, not argv — a `pkill -f` killed rustc children and left the cargo parent holding the lock; read `/proc/<pid>/environ` instead. Another session's target dir is fair game once its content is on main, nothing is uncommitted, and no process holds it — its *worktree* is not.

## Commits & shipping

- **Conventional commits** for every commit title.
- Implement features in full — no stubs or TODOs unless deferring an entire subsystem, and never defer scope without asking first.
- Commit each green, gated slice as it completes; do not wait for per-commit authorization. **Never `git push` without explicit authorization.**
- **This project does not use pull requests.** Work merges to `main` and CI runs on the push.
- **Cutting a release is `RELEASE.md`**, not something to reconstruct from the workflows. It carries the one ordering that matters — push the branch, wait for CI to go green on that exact commit, *then* push the tag, because the release's own gate runs after the tag exists — plus what the tag automates for you (the org `NOETA_VERSION` variable, the site dispatches, the extension publish), what you must never do by hand, and the post-release checklist for the `para` fleet.

## Before you're done

> [!IMPORTANT]
> **Run `scripts/gate.sh`, green, before you merge to `main`.** `main` is pushed in batches, so `.github/workflows/ci.yml` runs only at push cadence — many merges may land between one run and the next, and a red one is discovered on top of everything built since. `main` has twice sat red under `clippy -D warnings`, each time found by accident by an agent doing unrelated work.
>
> The script runs the CI jobs in CI's own split, reads every exit code directly, never pipes a gated command, runs every step even after one fails, and prints a per-step PASS/FAIL/SKIP table — **a SKIP is never a PASS**. Three tiers (measured on a 20-core box, `CARGO_BUILD_JOBS=8`, warm/cold):
>
> | | What it adds | Warm | Cold |
> |---|---|---|---|
> | `scripts/gate.sh --quick` | `cargo fmt --all --check` + both `clippy -D warnings` splits | 1m20s | 2m10s |
> | `scripts/gate.sh` | + workspace suite & oracles, lean-CLI and feature shapes, doc samples, JIT differentials, the `#[ignore]`d real-socket hot-reload suites (`scripts/hot-e2e.sh`), the instructions-retired perf ratchet | ~20 min | 42 min |
> | `scripts/gate.sh --full` | + wasm portability/differential, the linked `--native` AOT differential, miri, editor tooling | +2 min and up | — |
>
> `--quick` is the inner loop, the default is the merge gate, `--full` is for a release tag. Useful flags: `--list` (print the plan), `--only <substring>` (one group; it overrides the tier), `--install-hook` (opt-in `pre-push` running `--quick` — **not recommended here**: hooks live in the *common* git dir, so installing one changes every worktree of this repo including other agents', and no hook fires on the fast-forward merge into `main`, which is the moment we care about). It honors `CARGO_TARGET_DIR`/`CARGO_BUILD_JOBS` and sets neither.
>
> **A green merge tier does not predict a green CI, and the gap is a known short list.** Four checks exist only in `.github/workflows/ci.yml` or at `--full`: the **docs site** (builds `noeta-docs` against this checkout's `docs/`, so a broken link or anchor fails here and nowhere else), **miri**, **editor tooling**, and the **wasm** job — whose browser smoke and `wasi:http` e2e embed Noeta source in `.mjs` and `.sh` files that nothing at the merge tier compiles. Those embedded fixtures are the `.noe`-in-a-`#[test]` hazard in a form even grep for `.noe` will not find: a checker that gets *stricter* rots them, and they are only ever run by CI. Both of the reds that reached `main` in one week came from this list — a stale anchor, and a smoke fixture whose `?` had been type-checking only because a type argument was being erased. After a checker or inference change, run `--full`, or at minimum `node crates/noeta-playground/tests/browser_smoke.mjs <the built .wasm>`.

- Zero compiler warnings — `cargo build` produces no `warning:` lines.
- New functionality has tests; a coverage drop on a touched file is a regression.
- Architectural or feature changes update the docs *in the same commit*, following [Writing docs](#writing-docs). Keep aligned: `README.md` (newcomer entry), `ARCHITECTURE.md` (technical overview), `CONTRIBUTING.md` (developer entry, references rather than repeats), `AGENTS.md` (this file — conventions and workflow, **not** an architecture dump), `docs/` (the wiki page for the feature, with roadmap-only items marked as such), and the touched crate's `README.md`.
