# AGENTS.md

## Project Overview

This project is **Noeta**, a **new programming language, built from scratch in Rust** — a persistent, reactive runtime with a real type system, deployable to any surface (CLI, web, desktop) as a single binary.

The language and its design are documented in the `docs/` wiki (GitHub-Wiki format: onboarding, bundled tools, the language & stdlib reference, a concepts/design deep dive, and a contributing guide; start at `docs/Home.md`). The implementation overview is in `ARCHITECTURE.md`. The work tracker is `plans/` (start at `plans/roadmap.md`).

> [!NOTE]
> **Status.** The language core is complete and the toolchain has grown well past the original M1/M2 milestones. Shipped: the register-based bytecode **VM** + a Cranelift Tier-1 **JIT**, NaN-boxed values, shape-based object model, precise refcount + cycle **GC** (drop-at-last-use + in-place reuse over an ANF IR), the inferred-static bidirectional **type checker**, a **salsa** incremental query graph, traits/derives/generics/attributes+reflection, multi-file **modules**, and a layered **stdlib** over a twelve-capability `Host` boundary (sandbox + real disk/network). On top of that: native **AOT** builds, a **package manager** with keyless Sigstore signing, editor & agent tooling (**LSP/DAP/MCP**), a canonical **formatter** and **profiler**, **reactivity** (signals/computed/effects), local-first **CRDTs**/p2p, and OTLP **telemetry**. The differential oracle is **Core-IR interpreter ↔ VM**; the original M0 AST tree-walker was retired in the memory-management migration (`noeta-eval` survives as the IR interpreter). Open work is tracked in `plans/` (`roadmap.md` + `backlog.md`); completed arc ledgers live in git history.

## The compilation pipeline

```
source ─► noeta-lexer ─► tokens ─► noeta-parser ─► AST ─► noeta-check ─► noeta-ir ─► noeta-ir-passes ─┐
                        ┌───────────────────────────────────────────────────────────────────────────┘
                        ├─► noeta-eval ──────────────────────────► RunResult   (Core-IR interpreter, the oracle)
                        └─► noeta-compiler ─► Chunk ─► noeta-vm ─► RunResult   (register VM)
                                   (noeta-diagnostics renders every stage's typed Diagnostics)

  The lex→parse→check→compile path is also exposed as a salsa query graph (noeta-db):
  SourceProgram (input) ─► tokens(db) ─► ast(db) ─► checked(db) ─► bytecode(db).
  The checker (noeta-check) is a shared front-end: programs with type errors are
  rejected before either backend runs, so both stay observably identical.
```

Both backends implement `noeta-backend::Backend`. The conformance harness runs a program through both and asserts identical `RunResult`s — the **differential oracle** (`cargo run -p noeta-conformance -- --differential`, the dev binary). The Core-IR interpreter (`noeta-eval`) is the reference; the VM must reproduce it, and both execute the *same* RC-annotated IR.

Each stage is its own crate with explicit input/output types and no hidden shared mutable state, so a change is local to one crate and verifiable by that crate's tests.

## Crate map — where each change goes

| Crate | What it does (in → out) |
|---|---|
| `noeta-span` | Spans/source map (shared vocabulary), plus `PackageMap`/`PackageOrigin` — which package each source of a merged program came from, keyed by `SourceId`. |
| `noeta-diagnostics` | The one error catalog (`DiagnosticCode`, stable `E0xxx`) + the single `ariadne` renderer. Add a new diagnostic here. |
| `noeta-ast` | AST node types (pure data, every node carries a `Span`) + `SyntaxKind`. Add a new node here. |
| `noeta-lexer` | Source → tokens (`logos`). |
| `noeta-parser` | Tokens → AST (`chumsky`, error recovery). |
| `noeta-backend` | The `Backend` trait + `RunResult` — the seam both runtimes implement. |
| `noeta-eval` | Core-IR → `RunResult`, the **differential-oracle reference** (`Rc`-based value model). Began as the M0 AST tree-walker; that walk was retired in the RC migration and it now interprets the same RC-annotated IR the VM runs. |
| `noeta-object` | Shapes (hidden classes): `Shape`/`ShapeKind`, the flat-slot layout descriptor for records/classes/enums. Pure data; sits *below* `noeta-value` (which holds `Rc<Shape>`). |
| `noeta-value` | The M1 NaN-boxed `Value` + operator semantics; heap strings, closures, lists/maps, shaped objects/enums, the M2.5 `Payload::FileHandle` (a GC-leaf wrapper around the shared `noeta_stdlib::FileHandle`, so it depends on `noeta-stdlib`), and the F1 `Payload::Cell` + closure upvalue cells (closures are now GC *nodes* — a closure→cell cycle is reclaimed by the collector). The NaN-boxing `unsafe` lives here (miri-gated; the other `unsafe` opt-outs are listed in `ARCHITECTURE.md`). |
| `noeta-gc` | Refcount/`__destruct` GC policy + Bacon–Rajan cycle collector over `noeta-value` (now exercises closure/cell cycles). |
| `noeta-bytecode` | The register IR: `Op` (incl. F1 `MakeCell`/`CellGet`/`CellSet`/`UpvalueGet`/`UpvalueSet` and `MakeClosure` captures), `Chunk` (a function prototype), `Module` (the proto table + shape/method tables), disassembler (pure data). |
| `noeta-compiler` | AST → `Module` (returns `Unsupported` outside the VM's current subset). Closure conversion (celled locals + upvalue layout) lives in `freevars.rs`. |
| `noeta-vm` | `Module` → `RunResult` (M1 frame-based register VM, `VmBackend`; Tier 0). Add VM behavior here. Owns the Tier-1 promotion counters and the JIT's runtime helpers (`jit` feature). |
| `noeta-jit` | The Tier-1 Cranelift method JIT (`jit` cargo feature): hot prototypes → native code, registers in SSA, bail-to-interpreter deopt. Gated by its own `--jit-differential` oracle (byte-identity + zero leaks + zero refcount anomalies); the sandbox/differential baseline never runs it. |
| `noeta-builtins` | The prelude. |
| `noeta-conformance` | The test harness (`// expect:` runner, JSON, `--stage`/`--file`, `--differential`). |
| `noeta-cli` | The `noeta` binary. Core verbs `run`/`build`/`check`/`repl`, `test`/`bench`/`doc`, `dump`/`fmt`/`profile`/`cache`, the editor/agent servers `lsp`/`dap`/`mcp`, and the package-manager verbs `add`/`update`/`publish`/`audit`/`key` (plus the dynamically-wired `serve`). |

| `noeta-db` | The salsa (0.27) query graph: `SourceProgram` input → memoized `tokens`/`ast`/`checked`/`bytecode` queries, plus the M1.9.3 module graph — a `Workspace` input (entry + sibling sources) → `linked`/`linked_checked`/`linked_bytecode` (resolve+merge then check/compile the whole program), so editing one module recomputes only dependents. Carries the crate's one small `unsafe` (always-replace `Update` for foreign-result newtypes). |
| `noeta-types` | The `Type` lattice (pure data): primitives, `List`/`Map`/`Option`/`Result`, named/`Fn`, the gradual top `Unknown`, and the `?T` → `Option<T>` desugar. Also the built-in trait registry (`BuiltinTrait`/`BUILTIN_TRAITS`) the checker validates `impl`/`@derive` against. |
| `noeta-check` | The gradual type checker (`check(&Program) -> Vec<Diagnostic>`), the `checked` query's body. A shared front-end run upstream of both backends: exhaustiveness (E0011), `?`-typing and the `?` **position rule** over both halves — an `Option` needs a `?T` return, a `Result` needs a `Result<T, E>` one (E0012; a deferring return still defers, and an `Err` that reaches the top aborts at runtime as E0069) — arithmetic mismatch (E0007), unknown-type resolution (E0013, lit up once modules give type names referents), trait/derive validation (E0014 unknown trait, E0015 invalid impl), coherence (E0027 duplicate implementation, E0070 the **package orphan rule** — an `impl Trait for Type` must live in the package that declares the trait or the type, judged from the loader's per-source `PackageMap`), and data-attribute validation (E0017 invalid attribute). |

| `noeta-loader` | Multi-file module loading + linking (M1.9): walks the entry's package, **derives** each file's module path from where the file sits (`derive.rs`: the consumer's import prefix + the package-relative path, `/` → `.`, case verbatim, a leading `src/` dropped, a stem collapsing when it repeats the segment before it), resolves every `use` to real declarations honoring `pub` visibility, and merges them into one `Program` both backends run unchanged. `namespace` is still accepted but only as a restatement of the derived path — E0072 when it disagrees, E0073 when two files derive one path, E0074 for a name that is not a legal path segment. A `use` no module provides falls back to the M0 opaque stub. Import errors: E0019 (private/missing export), E0020 (name collision with another import or a local declaration). Carries the merged program's per-source side-tables the merge would otherwise destroy: `editions` and `packages` (the provenance the E0070 orphan rule reads). |

| `noeta-stdlib` | The layered standard library (M1.10). Where a Ring 1 operation is expressible over data represented *identically* in both runtimes, its semantics live here once and both backends call in — so the differential holds by construction. The string surface (`string_method` over `Arg`/`Output`/`Dispatch`: `upper`/`lower`/`trim`/`contains`/`starts_with`/`ends_with`/`split`/`replace`/`repeat`) is the first such; the two backends are reduced to thin value↔primitive glue at their existing built-in-method dispatch site (no compiler/bytecode change). Collection methods (list `reverse`/`contains`/`join`/`sorted`/`slice`/`first`/`last`/`to_set`, map `keys`/`values`/`has`, set `contains`/`union`/`intersection`) are Value-specific so each backend implements them, but the method *set* is the shared `ListMethod`/`MapMethod`/`SetMethod` enum (exhaustive `match` ⇒ neither backend can omit a method). `first`/`last` return a built-in `Option` constructed identically in both backends. A `Set` is a third heap value type (tree-walker `Value::Set`, VM `Payload::Set`) stored in canonical sorted+deduped form so display/iteration/equality are deterministic by construction; built via `[..].to_set()`, renders `{1, 2, 3}`, iterable/`len`-able. **Ring 2** native modules are imported with `use std.{name}` (explicit, so unused modules tree-shake) and dispatched `name.func(...)` through `call_method`; the module value is `Value::NativeModule`/`Const::NativeModule`. Five modules: `json` (`parse`/`stringify`, shared `Json` tree via `serde_json`); `math` (pure scalar functions, semantics shared in `math::call`); `random` (seeded SplitMix64 pure stepper, each backend threads a `u64` state); `fs` (file IO over a per-run in-memory `fs::Vfs` sandbox — write/read/exists/remove/list, M2.4 append/read_lines, M2.5 directory model `mkdir`/`is_dir`/`list(dir)` and `open` cursor handles); `time` (a logical monotonic clock, a per-backend counter, not wall-clock). All determinism-safe by construction. Misuse → `E0007` (arity/type/invalid-JSON/empty-range), `E0016` (`slice` bounds), `E0005` (unknown module function), or `E0021` (`fs.read` of a missing path; closed/wrong-mode handle; unknown open mode) via shared error builders. **M2 host seam:** all host-coupled effects (fs incl. directories, PRNG, clock, and the M2.2 `env`/`args` introspection) go through the `Host` trait (`host.rs`); `SandboxHost` is the deterministic in-memory impl the differential always runs. **M2.5 `handle.rs`:** the `fs.open` cursor file handle (`FileHandle` + `FileMode`/`Flush`/`FileHandleMethod`) — the whole mutable cursor state machine lives here once, so the tree-walker's `Rc<RefCell<FileHandle>>` and the VM's `Payload::FileHandle` advance it identically. |
| `noeta-host-real` (M2.3) | The real host: `RealHost` performs real-disk file IO over a per-isolate `tokio` `current_thread` runtime (driven async, blocked-on at the leaf — no async surface yet) and reads the real process `env`/`args`; PRNG/clock stay deterministic. The M2.5 directory methods map onto `tokio::fs` `read_dir`/`create_dir_all` + `Path::is_dir`. Implements `noeta_stdlib::Host`. The CLI/`noeta run` constructs it and runs the program on the IR path; the conformance differential keeps `SandboxHost`, so the real host is never compared backend-to-backend. `unsafe`-free. |

| `noeta-ir` / `noeta-ir-passes` | The ANF Core IR + precise-RC passes (liveness → drop insertion → in-place reuse). Both backends execute this IR; add memory-management behavior here. |
| `noeta-ext-abi` | The extension **ABI**: the `Host` supertrait (twelve capability traits), `ExtModule`/`ExtType`/`ExtFn` registration, and the neutral `NativeValue`/`PackedView` marshalling seam. Third-party native packages link against this. |
| `noeta-cache` | Default-on bytecode cache (`~/.cache/noeta/*.noeb`, build-identity-keyed). |
| `noeta-reactive` | Signals/computed/effects: the reactive graph, topological flush, E0045 runaway guard. |
| `noeta-jit-abi` | The frozen native↔interpreter calling-convention vocabulary shared by `noeta-vm`/`noeta-jit`. |
| `noeta-aot-runtime` / `noeta-bundle` | Native AOT builds (`noeta build --native`): runtime support + self-contained artifact bundling (per-ring stdlib + DCE). |
| `noeta-ide` | Shared IDE engine (hover/def/refs/outline/call+role graph) over the salsa db; reused by the LSP and MCP servers. |
| `noeta-lsp` | `noeta lsp`: tower-lsp language server. |
| `noeta-dap` | `noeta dap`: debug adapter driving the production VM via a per-op debug hook. |
| `noeta-mcp` | `noeta mcp`: agent-native MCP server (reflection manifest + ~27 tools over stdio). |
| `noeta-fmt` | `noeta fmt`: the canonical formatter (also drives LSP formatting). |
| `noeta-prof` | `noeta profile`: dev profiler/flamegraph over the tier-0 VM. |
| `noeta-pm` | The package manager: manifest/lockfile, path/git/registry resolution, keyless Sigstore signing + provenance, native-package toolchain composition. |
| `noeta-alloc-probe` | Test-only global-allocator probe for heap-residency assertions. |

M1 is complete (slice history in git, under the former `plans/m1/`): the bytecode VM + NaN-boxed values + shapes + cycle GC (M1.0–M1.6), the salsa query graph (`noeta-db`, M1.1), the gradual type checker (`noeta-types`/`noeta-check`, M1.7), the trait system (M1.8), multi-file modules (`noeta-loader`, M1.9 — `pub` visibility, E0019/E0013/E0020, and the module graph as salsa queries: `noeta-db`'s `Workspace`/`linked`/`linked_checked`/`linked_bytecode`), and the layered stdlib (`noeta-stdlib`, M1.10 — full Ring 1 + Ring 2 `json`/`math`/`random`/`fs`/`time`) have all landed. The `noeta-host-real` crate landed in M2.3 (the real host). Everything once listed as deferred here — the HTTP `server` (`noeta serve`) and the `lsp` — has since shipped; see the status note above, `plans/roadmap.md` for the current frontier, and `plans/backlog.md` for every open item.

## The new-feature template (the standard shape of a change)

A language feature is added as a **vertical slice** in this order (the slice template lives in `plans/README.md`):

1. **Grammar / AST** — token(s) in `noeta-lexer`, node(s) in `noeta-ast`, production in `noeta-parser` (keep surface sugar as its own AST node).
2. **Checker rule** — typing/inference in `noeta-check` (+ a new `Type` form in `noeta-types` if needed); add a negative conformance case for any new static-error class.
3. **Lowering** — AST → IR in `noeta-ir` (+ `noeta-ir-passes` if the feature introduces owned heap values that need drops/reuse).
4. **Both backends** — evaluation in `noeta-eval` (the oracle) **and** bytecode in `noeta-compiler`/`noeta-vm`; keep any shared semantics in `noeta-stdlib` so the two agree by construction.
5. **Conformance cases** — `tests/conformance/**.noe` with `// expect:` headers, including negative/error cases; must run `--differential` at `0 skipped`.
6. **Snapshot update** — `insta` snapshots (tokens / AST / rendered diagnostics), reviewed, never blind-accepted.

**The iron rule: every feature or fix lands with a conformance corpus entry.** Prefer vertical-slice tasks ("implement `~` end-to-end") over diffuse refactors — a slice's done-condition is "its conformance cases pass."

## Naming

- Files: `snake_case.rs`
- Types: `PascalCase`
- Functions/variables: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
  
## Spelling

Use **American English** throughout: code comments, doc comments, and documentation. For example: `sanitization` not `sanitisation`, `behavior` not `behaviour`, `specialized` not `specialised`.

## Enums & Constants Over Magic Strings

Prefer enums and constants over raw string literals. Variant names, format identifiers, provider names, severity levels, and similar fixed sets should be modeled as enums with `Display`/`FromStr` impls (or `strum` derives) rather than compared as ad-hoc strings. 

## Formatting & Linting

- **`cargo fmt --all`** — format the entire workspace with `rustfmt`. All code must be formatted before committing.
- **`cargo clippy -- -D warnings`** — run Clippy with warnings-as-errors. Fix all diagnostics; do not `#[allow]` them without justification.
- No custom `rustfmt.toml` — we use the default `rustfmt` style.
- **The CI toolchain is pinned to `@1.97.0`** (in every `ci.yml` job + `release.yml`; miri stays nightly). Clippy lints are version-sensitive, so lint against 1.97.0 locally — a floating `@stable` will silently add lints that only surface as a red CI you didn't cause.

## Design Patterns

- Keep a performance oriented architecture in mind, follow SOLID and keep code DRY.
- Where applicable (ie. not in a data oriented context), take inspiration from DDD to keep code maintainable.
- Avoid god-classes, prefer DI and the strategy pattern.

## Documentation

The following documentation files should always be kept up to date.

- `README.md` serves as a starting point for newcomers, introducing the project, directing users to the wiki and developers to `CONTRIBUTING.md`. If project setup or basic architecture changes, align these files.
- `AGENTS.md` serves as the entry point for coding agents, providing a comprehensive overview of conventions and a very high-level architectural overview so they know where to find more details.
- `CONTRIBUTING.md` serves as the entry point for developers, less heavy on the details than `AGENTS.md` and instead referencing external documents rather than repeating it. 
- `ARCHITECTURE.md` should reflect a thorough technical overview of the system architecture, giving agents and humans necessary technical context.
- `docs/` is the wiki: it comprehensively documents the language and all of its features, following GitHub Wiki conventions (flat pages, `_Sidebar.md` navigation, synced to the project wiki on push). Its five sections are onboarding, bundled tools, the language & stdlib reference, a concepts/design deep dive, and a contributing guide; start at `docs/Home.md`. The target audience is developers wanting a fresh take on modern DX. When a feature changes, update the relevant wiki page alongside the code, and mark roadmap-only items as such.
- Each crate should have its own `README.md` that there instead serves as the primary documentation of that crate.

> [!NOTE]
> Markdown should never have hard line wrap.

## Agent Workflow

Follow these practices when working on this codebase as an AI coding agent.

### Before You Start

- Read this file and the module layout to orient yourself.
- Use the codebase — search, read files, check types — before making assumptions about how something works.
- When a task spans multiple modules, plan the full set of changes before editing.

### While Working

- Build after every meaningful change (`cargo build`). Fix errors before moving on.
- Keep the compiler warning-free. Do not introduce new warnings.
- Evaluate whether one should refactor files when they grow large.


### Testing

This project is primarily developed by coding agents, so its imperative that we maintain a high quality and high coverage test suite.

**Coverage.** Measure with `cargo-llvm-cov`, never `cargo-tarpaulin` (tarpaulin can't see across the process boundary, so it reports the subprocess-driven CLI tests in `crates/noeta-cli/tests/` as 0% coverage of the `noeta` binary).

```sh
cargo llvm-cov --workspace --summary-only   # per-file line/region/function summary
cargo llvm-cov --workspace --html           # browsable report under target/llvm-cov/html
```

Setup if missing: `rustup component add llvm-tools-preview && cargo install cargo-llvm-cov`. Treat a coverage drop on a touched file as a regression to fix, not to ignore.

> [!IMPORTANT]
> **A language change silently rots hand-written fixtures that have no expected-output file.** The conformance corpus is protected (every case pins its `// expect:` output), but Rust in-test fixtures (fixtures embedded in `#[test]`s) and the programs under `examples/` are not — they just stop compiling under the new rule with nothing asserting they still should. After any language change, check these first. Examples run as their own gated CI step: core examples inline, but **package examples are `#[ignore]`d and serial** (each composes its own toolchain, and concurrent compositions corrupt the shared cache).

**Benchmarks (M2.0+).** `cargo bench -p noeta-vm` runs the `criterion` benches over the VM hot paths (dispatch loop, property access through inline caches, allocation) in `crates/noeta-vm/benches/vm.rs`. VM-touching changes should check no regression against the baseline; positioned as the last/scheduled CI gate (implementation-plan §6.6/§6.7).

### Version Control and Continuous Work

Commit as you go and always implement features in full, no stubs or todos unless deferring entire subsystems. When a task is clear, work independently and verify changes using the comprehensive test suite. Commit each green, gated slice as it completes — do not wait for per-commit authorization — but **never `git push` without explicit authorization**.

This project is currently pre-alpha and not public, so you don't need to worry about pull requests.

> [!IMPORTANT]
> **ALWAYS create a new branch AND an isolated git worktree before editing any file — never work in the shared root checkout.** Many agent sessions run concurrently against this same repo. The root checkout `/home/niklas/Code/lang` floats between branches and is actively edited by other sessions; working there causes real collisions — a parallel session's `git add -A && git commit` will sweep your uncommitted files into *its* commit, and a stray `git reset --hard`/`git restore` destroys *their* live work.
>
> Start every task with:
> ```
> git worktree add -b <branch> .claude/worktrees/<name> <base>
> ```
> then `cd` into it and do all editing, building, and committing there (`<base>` is usually `main`, or another session's HEAD when continuing from it). If work has already leaked into the root, recover non-destructively: `git reset` (unstage only, never `--hard`), branch a worktree from the polluting commit, finish there, and `git restore` only your *own* stray files out of the root.

> [!WARNING]
> **The parallel-worktree setup has sharp edges that have each burned an agent for real. Internalize these:**
>
> - **The Bash working directory silently resets to the shared root `/home/niklas/Code/lang`** (typically after a command that `cd`s elsewhere; a "Shell cwd was reset to …" line is easy to miss). A relative-path edit then lands in the *root* instead of your worktree, and `git`/`cargo` then report the *root's* state — which has looked like "phantom compile errors" and "a lost HEAD" that were never real. **Use absolute paths for every file and `CARGO_TARGET_DIR`, and confirm the directory (`cd <worktree> && pwd && git log --oneline -1`) before trusting any `git`/`cargo` output.** Prefer the Edit/Write tools (cwd-independent) over shell heredocs for source edits.
> - **`git stash` is shared per-repository across all worktrees** — never use it as a "clean tree" control. A pop can conflict with, or surface, another agent's stash, and it does not stash committed work anyway. To compare against a baseline, check out the commit in a separate throwaway worktree.
> - **A scratch directory outside the repo is shared far more widely than it looks** — measured: agent scratch and task-output paths are keyed by *project directory*, not by session, so four concurrent sessions on this repo share one directory; and `/tmp` is of course global. Two agents that both write `suite.log` there will truncate each other's, and the second one to read it sees the *other's* result under its own name. This has already produced a wrong claim in a report, and it is worse for a *diagnosis*: a foreign red read as your own gets chased for an hour before anyone checks the binary path in the error text. **Put your branch name in any filename you write outside your worktree**, or write inside it. When a failure surprises you, read the paths in its output before believing it is yours.
> - **Ship from the *main* checkout, not the worktree.** `git merge <branch>` run from inside that branch's own worktree is a silent no-op ("Already up to date"), and `git worktree remove` then deletes your own cwd. Split the ship step: (1) commit inside the worktree; (2) `cd /home/niklas/Code/lang` as a fresh command; (3) merge + `git worktree remove <path>` from there.
> - **Other sessions' worktrees under `.claude/worktrees/` may hold live, uncommitted work.** Before removing any worktree or deleting any branch, check `git status --porcelain` in it and confirm with the user — a branch whose tip is merged may still carry the next feature on top.

> [!IMPORTANT]
> **Use a per-agent `CARGO_TARGET_DIR` — a shared target dir causes last-writer-wins rlib contamination (identical-version path crates) and phantom test failures.** Target dirs are ~70–210 GB *each* and `/home` (450 GB) fills fast with several worktrees active — a full disk has **truncated a source file mid-write**. Check `df -h /home` before a long build, and **do not build in `/tmp`** (a 14 GB tmpfs, far too small — the build dies partway). Deleting a worktree's *own* `target/` is safe, but never another agent's while they may be mid-build.

> [!IMPORTANT]
> **Reclaim your target dir the moment your work merges — and use `git cherry`, not `merge-base`, to decide what has merged.** A finished-but-unpruned worktree is the single largest thing on this disk: two of them once held **267 GB** (a 132 GB cache plus a 107 GB in-tree `target/`), and 201 GB more sat in a merged branch's cache, idle for five hours. Left alone they took `/home` to **88% full**, where NVMe write throughput collapses and every concurrent build stalls on I/O rather than CPU — measured: load 112 with **0 runnable, 19 blocked, 60% iowait, 34% idle**. Three separate "everything is slow" incidents in one day had three different causes (CPU oversubscription, then a suspected lock, then this), and only the last one was disk. **Prune all three together: branch, worktree, and `CARGO_TARGET_DIR`.**
>
> Two traps in deciding *what* is safe to reclaim:
>
> - **`git merge-base --is-ancestor` compares hashes, so it reports rebased-and-landed work as unmerged.** Two branches here looked like stranded fixes by that test; `git cherry main <branch>` marked both `-` (already upstream) and `git patch-id --stable` gave byte-identical ids to their main-side twins. They were rebase residue, and an hour went into "recovering" content that had shipped days earlier. Use `git cherry` (or patch-ids) to ask "is this content on main?"; `--is-ancestor` answers a different question.
> - **`pgrep -f` will not find the owner of a target dir, because cargo carries `CARGO_TARGET_DIR` in the *environment*, not argv.** A `pkill -f "<name>"` killed rustc children and left the cargo parent holding the lock. Check `/proc/<pid>/environ` instead. Another session's target dir is fair game once its content is on main, nothing is uncommitted, and no process holds it — but its *worktree* is not (see the note above: a merged tip may still carry the next feature).

> [!IMPORTANT]
> **Also set `CARGO_BUILD_JOBS` — isolating target dirs fixes correctness and makes throughput worse.** Cargo defaults to one job per core, so *each* concurrent build claims the whole machine: measured here, three agents on a 20-core box drove load to **60** with 51 rustc processes, none blocked on a lock and every build running at a third speed. That reads exactly like a deadlock and is not one. Divide the cores by the number of agents you expect and set it — `CARGO_BUILD_JOBS=6` for three agents on 20 cores lands near full utilization with no thrashing. Before diagnosing a "stuck" build, check `nproc` against `uptime`'s load and the process states (`S<l` is running, not blocked): oversubscription and lock contention look identical from the outside and have opposite fixes.

> [!NOTE]
> We follow conventional commits for all commit titles and PRs.

### Before You're Done

- Verify zero compiler warnings (`cargo build` should produce no `warning:` lines).
- Run `cargo fmt --all` and `cargo clippy -- -D warnings`. Fix any issues.
- Run the full test suite and confirm all tests pass.
- If you added new functionality, add tests for it.
- If you made architectural changes or added new features, make sure documentation is up to date.

