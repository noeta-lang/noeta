# Developer-Tooling Layer Audit (noeta-cli, noeta-ide/lsp/dap/mcp/db, noeta-fmt, runners)

All paths relative to `/home/niklas/Code/lang/.claude/worktrees/audit-main`.

---

## Finding 1 — The "launch a program" pipeline exists in five flavors with different capability envelopes; DAP, MCP-debug, and `repl --load` cannot see dependency packages

**Severity: high**

**Evidence:**
- `crates/noeta-dap/src/session.rs:65` — `compile_file` loads via `noeta_loader::load(path, root_edition(path))`. No `graph::resolve_graph`, no `load_with_deps`, no tier/target activation. The module doc (`session.rs:1-8`) claims "This is the *same* production pipeline `noeta run` drives" — it is not: `run` resolves dependency packages.
- `crates/noeta-mcp/src/debug.rs:472` — same plain `noeta_loader::load` for the `debug_*` tools.
- `crates/noeta-cli/src/lib.rs:5745` (`repl_bootstrap`) — same plain `noeta_loader::load`.
- Contrast the dep-aware paths: `crates/noeta-cli/src/lib.rs:4419-4427` (`load_linked`: `graph::resolve_graph` → `load_with_deps`, used by test/bench/doc), `lib.rs:2514-2522` (`cmd_check`, same), `crates/noeta-runner/src/compile.rs` (`compile_whole_file` for run/dump/build, plus cache + tiers), and `crates/noeta-ide/src/lib.rs:471-508` (`resolve_dep_modules` via `noeta_pm::manifest::dependency_packages` into salsa `DepModule`s).
- `crates/noeta-mcp/src/analyze.rs:34-50` (`prepare`) also links without deps — that one carries a comment acknowledging it ("this path resolves no dependencies").

**Why it matters:** This is not just duplication — it is a user-visible behavior split. A program with a `use <dep>.…` import runs under `noeta run`, checks under `noeta check`, resolves in the editor — and then fails to load the moment the user sets a breakpoint (DAP), asks the agent to `debug_launch` (MCP), or does `noeta repl --load app.noe`. Five call sites each decided independently which of {siblings, dep packages, tiers, cache, edition} to thread; every future loading feature (a sixth concern) must be added in five places or silently diverge again — exactly how this gap appeared.

**Proposed remedy:** Extract one `load_project(entry, LoadOptions { deps: bool, tiers, target }) -> Linked` into `noeta-runner` (or `noeta-loader` itself, taking the resolved dep set as input to keep the loader pm-free), and route DAP `compile_file`, MCP `debug.rs`, `repl_bootstrap`, and `load_linked` through it. Deps-on should be the default; the debugger keeps its own `compile_with_sites(..., debug=true)` back half unchanged. This is incremental: DAP first (highest user impact), then MCP-debug, then REPL.

**Perf-regression risk:** low (dep resolution adds manifest/graph reads to debug launch — the same cost `run` already pays).

---

## Finding 2 — `crates/noeta-cli/src/lib.rs` is a genuine god-file: 6,336 lines mixing dispatch, five embedded engines, and a linker driver

**Severity: medium**

**Evidence:** One file contains: the clap `Command` enum (`lib.rs:65-529`), the dispatcher `run_cli` (`530-754`), the package-manager verbs (`cmd_scope`/`cmd_claim`/`cmd_add`/`cmd_update`/`cmd_publish`/`cmd_audit`/`cmd_key`, `757-1780`), fmt driving incl. per-directory tier scans (`1941-2239`), the run/check machinery (`2240-2644`), unknown-subcommand recovery (bare-file, tier-dispatch, external binary, `2667-3048`), a **parallel HTTP serve worker pool + hot-reload orchestration** (`serve_parallel_impl`/`run_worker_hot`/`run_program_hot`, `3193-3583`), the **entire AOT emit/link driver** (`emit_native`/`aot_ring_features`/`resolve_aot_runtime`/`link_native`, `3635-4397`), the `@test` runner (`4457-4998`), the bench runner + baseline persistence (`4999-5449`), doc generation glue (`5450-5711`), and the full REPL loop (`5712-6188`). Meanwhile `compose.rs`, `docgen.rs`, `watch.rs` prove the crate already knows how to split.

**Why it matters:** The serve worker pool, the AOT linker, the test runner, and the REPL are engines, not "thin glue" (the file's own header claims the binary is thin glue — true for `run`, false for the rest). Unrelated changes collide in one file; helpers with one caller sit hundreds of lines from it; the file is past the size where a contributor (or agent) can hold its invariants.

**Proposed remedy:** See the decomposition sketch at the end. Purely mechanical moves; no seam redesign required.

**Perf-regression risk:** none.

---

## Finding 3 — The tier-runner prologue is triplicated near-verbatim in `cmd_test`, `cmd_bench`, `cmd_doc`

**Severity: medium**

**Evidence:** `lib.rs:4466-4512` (test), `lib.rs:5030-5077` (bench), `lib.rs:5620-5680` (doc) each repeat the same ~45-line sequence: `compose::maybe_delegate` → `target_gate` → `load_linked` → `resolve_providers` (with identical error arms) → `activate_tiers_with(&linked.program, &[tier], &providers)` → the identical `match activated.registry.resolve_provider(...)` with the `run_declared_tier` escape → activation-diagnostics emit → `check_all_with_editions` gate. Only the tier name and the post-prologue body differ (doc adds a `providers.get("doc")` pre-filter).

**Why it matters:** Three copies of a policy-bearing sequence (order of gate → load → provider resolution → activation → check) that must stay in lockstep — the provider-dispatch escape hatch was clearly pasted into each. The next tier verb (or a change like "activation diagnostics should not gate when the provider is declared") is a three-site edit.

**Proposed remedy:** One `fn tier_prologue(file, tier: &str, target) -> Result<TierRun, ExitCode>` returning `{ linked, activated, checked }` or the already-dispatched `run_declared_tier` exit code (an enum `Prologue::Ran(ExitCode) | Prologue::Ready(TierRun)`). ~120 lines deleted.

**Perf-regression risk:** none.

---

## Finding 4 — `noeta check` over a directory re-links and re-checks every module once per importer (quadratic), then builds dedup machinery to hide it — while the salsa substrate built for exactly this sits unused

**Severity: medium**

**Evidence:** `lib.rs:2437-2564` — every `.noe` file under the path is treated as its own entry (`noe_files`), each entry runs `graph::resolve_graph` + `load_with_deps` (which lexes/parses the entry **and all its directory siblings**) + a full `check_all_with_editions`. The doc comment at `2429-2432` acknowledges it: "A module shared by several entries is therefore linked (and its diagnostics produced) once per importer; diagnostics are deduplicated globally by … file + span + code". The dedup `BTreeMap` with `Rc<SourceMap>` values (`2494-2507`) exists purely to mask the repeated work. `noeta-cli` has **no dependency on noeta-db at all** (grep: only a comment at `lib.rs:39` mentions it). Meanwhile `noeta-db`'s workspace family (`crates/noeta-db/src/lib.rs:327-647`) memoizes exactly the per-source `tokens_in`/`ast_in` this repeats, and `noeta-mcp/src/analyze.rs` proves the "build a workspace, read `linked_checked`" pattern is a dozen lines.

**Why it matters:** For a directory of N sibling files this is N full lex/parse passes over each file and N whole-directory checks — `noeta check` on a real project scales quadratically in directory size, and dependency graphs are re-resolved per file too. It also means CLI check and LSP diagnostics run on different substrates (loader-batch vs salsa), relying on the db's byte-identity test to stay honest rather than on shared execution.

**Proposed remedy (challenging a documented decision):** The documented rationale (an un-imported library module must still be checked) doesn't require per-entry re-linking. Incremental fix without salsa: group entries by directory, load each directory's sources **once**, then link each entry against the already-parsed sibling ASTs (`noeta_loader::link_parsed` already takes `&[&Program]` — the salsa `linked` query at `noeta-db/src/lib.rs:509-555` shows the exact shape). Better: build one `LangDatabase` per directory and read `linked(db, ws_for_entry)` per entry — parses memoize across entries for free, and check converges on the same substrate as the LSP/MCP.

**Perf-regression risk:** none (strictly less work); low risk of diagnostic-ordering churn, contained by the existing BTreeMap ordering.

---

## Finding 5 — The LSP runs the whole-workspace checker twice per document version: diagnostics on `linked_checked`, everything else on `linked_checked_ide`

**Severity: medium**

**Evidence:** `crates/noeta-ide/src/lib.rs:520` — `diagnostics()` reads `noeta_db::linked_checked`. Seven other features (`inlay_hints` :553, `hover_type` :584, definition-member path :793, references :887, completions :1048, tests :1698) read `linked_checked_ide`. `noeta-db/src/lib.rs:249-253` documents the intent for the ide flavor: "The LSP reads diagnostics *and* hover types from this one query — **a single checker run per document version**" and states "Diagnostics are identical between the two." Because `Checked`'s `replace_update!` PartialEq is always-false (`db/lib.rs:176-196`, documented no-backdating), every edit invalidates both queries; `publish_all` runs on every `didChange` (`noeta-lsp/src/lib.rs:1144-1155`), and inlay hints/semantic tokens are re-requested by the editor after each edit — so in practice **two full merged-program checker runs per keystroke per open document**.

**Why it matters:** The checker is the most expensive query in the graph; this silently doubles the per-keystroke latency floor, and it contradicts the db's own documented design (the single-file `checked_ide` honors it; the workspace family drifted when `linked_checked_ide` was added).

**Proposed remedy:** Point `DocumentStore::diagnostics` at `linked_checked_ide` (diagnostics are identical by construction, per the db's own contract) — a one-line change. Keep `linked_checked` for compile-path consumers (`linked_bytecode`, MCP `check`) that never want the expr-types index.

**Perf-regression risk:** low — the diagnostics path now records `expr_types`, but that cost was already being paid a second time by the inlay/hover queries; net is one run instead of two.

---

## Finding 6 — One salsa workspace per open document: the same files are ingested, parsed, linked, and checked once per open document

**Severity: medium** *(challenging a documented design)*

**Evidence:** `crates/noeta-ide/src/lib.rs:150-170` (`WorkspaceCache`, "one workspace per open document") and `:401-418` — `refresh_workspace` creates **fresh `SourceProgram` inputs** for the entry and every sibling per open document. Two open files in the same directory therefore hold two independent salsa input copies of every file in that directory; `propagate` (`:244-260`) dutifully pushes each keystroke into every copy, and `publish_all` (`noeta-lsp/src/lib.rs:705-715`) then recomputes `linked_checked` for **every** open document's workspace on every change. The `tokens_in`/`ast_in` family is keyed by `(ws, src)` (`noeta-db/src/lib.rs:464, 472`), so nothing memoizes across workspaces even for identical text.

**Why it matters:** Editor cost is O(open-documents × directory-size) per keystroke in both parsing and checking — with finding 5 stacked on top, a user with 6 files of one package open pays ~12 whole-workspace checker runs per keystroke. It also multiplies salsa storage (inputs are never collected — see finding 9).

**Proposed remedy:** Key workspaces by **directory** instead of by entry document: one `Workspace` per directory with shared `SourceProgram` inputs, and make the link entry-parametric — `linked(db, ws, entry)` instead of `linked(db, ws)` (the loader half already accepts any entry program; the query change is mechanical). Diagnostics for a given document filter by its `SourceId` exactly as they do today (`ide/lib.rs:523`). Incremental first step with no db change: share the sibling `SourceProgram` inputs across `WorkspaceCache`s (keyed by URI) so at least the single-file `tokens`/`ast` memoize once.

**Perf-regression risk:** none for the sharing step; the entry-parametric link needs the existing `SourceId::FIRST == entry` assumptions audited (`hover_type`, `diagnostics`, `inlay` all filter on `SourceId::FIRST`).

---

## Finding 7 — Salsa queries read hidden non-salsa inputs: the process-global extension registry

**Severity: medium** *(challenging a documented decision)*

**Evidence:** `crates/noeta-db/src/lib.rs:222-228` — the tracked `ast` query extends tier names from `noeta_stdlib::registry::ext_verbatim_tier_names()`; `:449-453` — `workspace_text_tiers` does the same ("the LSP/pipeline must seed them like the loader does"). `crates/noeta-ide/src/lib.rs:624` — `hover_namespace` reads `noeta_stdlib::registry::default_seeded()`. None of these are salsa inputs; a change to the installed extension set can never invalidate a memoized parse. The instance-registry arc made registries per-session for embedding (memory notes "LSP/MCP/IDE left on global as single-registry" — a recorded decision).

**Why it matters:** Today the global registry is installed once at process start (`run_cli` → `install_with_extras`, `cli/lib.rs:541`), so the value is constant per process and the queries are sound. But it is a *correctness landmine*: the moment an embedder builds a `LangDatabase` alongside a per-session registry (`Builder::with_extensions` already exists), memoized `ast`/`workspace_text_tiers` results silently encode the wrong tier set, and salsa gives no signal. The invariant "this global never changes after first query" is enforced nowhere.

**Proposed remedy:** Make the extension tier-name set an explicit salsa input (a field on `Workspace`, or a dedicated `#[salsa::input] ExtEnv`), populated by whoever constructs the db (DocumentStore, MCP `prepare`, wasm hosts). This also documents the dependency instead of hiding it. Cheap: the value is a `Vec<String>`.

**Perf-regression risk:** none.

---

## Finding 8 — MCP still carries its pre-M5 bespoke layer next to the shared engine: a second `LineIndex`, a second outline walk, and a third position convention

**Severity: medium-low**

**Evidence:**
- `crates/noeta-mcp/src/analyze.rs:78-133` — a `LineIndex` whose construction loop is byte-for-byte the same as `crates/noeta-ide/src/offsets.rs:53-68`, but 1-based/UTF-8-only where the ide one is 0-based/encoding-aware.
- `crates/noeta-mcp/src/understand.rs:370-400` + `symbol_node` — a full declaration-outline walk (`SymbolNode`: fn/struct/class/enum/impl with member children) parallel to `noeta_ide::symbols::outline` (`crates/noeta-ide/src/symbols.rs:67`), which the LSP serves. The M3 module doc (`analyze.rs:5-8`) explains the original reason ("the shared IDE engine … is extracted later, at M5") — M5 happened (`navigate.rs`, `trace.rs`, `understand.rs` docs half all ride `noeta_ide` now), but `symbols`, `type_at`'s span math, and `LineIndex` never moved over.

**Why it matters:** The agent's outline and the editor's outline are two implementations that can drift (kinds, nesting, detail strings — MCP adds `roles`, ide adds signature detail). The stated MCP design goal is "agent and editor can never disagree" (`navigate.rs:2-4`); `symbols` is the surface where they still can.

**Proposed remedy:** Rebase MCP `symbols` on `noeta_ide::symbols::outline` (+ the role annotation as a post-pass — the role join already uses shared `reflect::build`), and replace `analyze::LineIndex` with `noeta_ide::offsets::LineIndex` behind a thin 1-based adapter at the wire boundary.

**Perf-regression risk:** none.

---

## Finding 9 — The IDE/LSP concurrency story: no cancellation anywhere, one global Mutex, and salsa inputs that are never collected

**Severity: low**

**Evidence:** `grep Cancelled|cancel` over noeta-lsp/ide/db: zero hits. `noeta-lsp/src/lib.rs:527-531` — all requests serialize through `Mutex<DocumentStore>` (documented at `:11-12` as "synchronous, fast"). `noeta-ide/src/lib.rs:400-418` — a file-set change abandons the old `SourceProgram`/`Workspace` inputs and creates fresh ones; salsa inputs are never deleted, and `close()` (`:263-267`) drops only the map entry, so a long editing session monotonically grows the database.

**Why it matters:** "Fast" holds at corpus scale but is load-bearing with findings 5+6 stacked: one slow whole-workspace check blocks `didChange` and every hover behind the mutex, with no way to cancel a stale request (salsa's cancellation mechanism — set an input from another thread, `Cancelled` unwinds readers — is precisely designed for this and unused). The input leak is slow but unbounded.

**Proposed remedy:** Not urgent until workspaces grow, but the cheap first steps are: reuse inputs by URI on file-set change (update text, only *add* genuinely new files) which fixes most of the leak; later, adopt the standard salsa LSP pattern (snapshot the db per request on a worker, mutate inputs on the main loop to cancel in-flight reads).

**Perf-regression risk:** none.

---

## Finding 10 — `noeta fmt`'s per-directory tier discovery is a mirrored pair of scans, each re-lexing the directory and re-resolving the dependency graph

**Severity: low**

**Evidence:** `crates/noeta-cli/src/lib.rs:2056-2137` — `fmt_text_tiers` and `fmt_tier_formatters` each do the identical `read_dir` → read → lex sweep of every sibling, and each independently calls `graph::resolve_graph(entry)` (a full dependency resolution) and re-lexes every dependency module; the second function's comment says outright "Mirrors `fmt_text_tiers`'s scan so the same tiers are seen." Both run once per directory per `cmd_fmt` invocation (cached in per-dir maps at `:1969-1988`). This is also the third implementation of "the project-wide text-tier union" (besides `noeta_db::workspace_text_tiers` at `db/lib.rs:441-457` and the loader's own seed).

**Why it matters:** Two full lexes of every file in the project (plus two dependency resolutions) before formatting begins, and a comment-enforced ("mirrors") invariant instead of shared code. If the tier-declaration syntax grows, three sites change.

**Proposed remedy:** One scan producing `(TextTiers, TierBodyFormatters)` — trivially mergeable since both iterate the same sources and both end on the extension registry. Longer term, a shared `noeta_loader::project_text_tiers(sources)` used by loader, db, and fmt.

**Perf-regression risk:** none (halves the scan).

---

## Finding 11 — REPL prompt entries are edition-blind

**Severity: low**

**Evidence:** `crates/noeta-cli/src/lib.rs:6085-6086` (`repl_step`): `let lexed = lex(&source); let parsed = parse(&source, &lexed.tokens);` — the plain, default-edition entry points, while every other tooling surface threads the package edition (`format_source_in`, `edition_of_uri`, the db's `edition_of`, `repl_bootstrap`'s own `manifest::root_edition` at `:5745`). A `repl --load` inside a future non-default-edition package would check/compile the bootstrap under the package edition but parse subsequent prompt fragments under the default.

**Why it matters:** Zero impact today (one edition exists), but it is exactly the class of gap the editions arc set out to close ("edition threaded through lex/parse/link/cache/salsa/LSP/fmt"), and it will be invisible until the first real edition ships.

**Proposed remedy:** Resolve `root_edition(cwd)` once in `cmd_repl` (or take the `--load` file's edition) and use `lex_in`/`parse_in` in `repl_step`.

**Perf-regression risk:** none.

---

## Finding 12 — `cli/tests/cli.rs` is a 6,063-line single test binary — sectioned, but one namespace

**Severity: low**

**Evidence:** 193 test fns in one file, organized only by banner comments (`// --- run ---` … `// ===== namespace-protection arc tests`, lines 68-5465). The serve/hot/watch paths already split into 9 sibling integration files with a shared `tests/common/`.

**Why it matters:** Mostly navigational: unrelated arcs (jit-stats, package manager, namespace protection) share one compilation unit and one flat fn namespace; a failure filter (`cargo test --test cli pm_`) works only by naming discipline. Note the single-binary choice does have a real upside here — each extra integration binary re-links the full `noeta` dependency tree — so splitting into *modules within one binary* (`tests/cli/main.rs` + `mod run; mod check; mod pm; …`) gets the structure without the link-time cost.

**Proposed remedy:** As above; mechanical.

**Perf-regression risk:** none.

---

# What's already good

- **noeta-lsp is a genuinely thin adapter.** All 1,424 lines are wire conversions, capability registration, and lock-scoped delegation to `DocumentStore`; its Cargo.toml depends on nothing below `noeta-ide` + `noeta-diagnostics`. The "extract the engine at M5" promise holds completely on this surface.
- **The tooling-unification claims verify where made.** MCP navigation/trace/docs/guide ride `noeta_ide` (`navigate.rs`, `trace.rs:12-18`, `understand.rs:194`, with a test asserting MCP-docs ≡ LSP-docs at `understand.rs:549-565`); the DAP debug console is the REPL engine (`VmSession::adopted`, `SessionCompiler`/`SessionChecker` threading in `dap/session.rs:20-31`); formatting is one engine everywhere (`format_source_in` from CLI, LSP, MCP); check-diagnostics JSON is one shape (`noeta_diagnostics::to_json` + the `schema` feature so MCP `check` ≡ `noeta check --format json`).
- **run/dump/build/cache share one front-end** — `noeta_runner::compile_whole_file` (`runner/compile.rs`), extracted with an explicit drift-firewall rationale, and the disassembly/bundle/exe/native emitters all consume the same module. The dev-toolchain vs shipped-runner split (compose.rs `ShimKind`) is a thoughtful security/size boundary.
- **noeta-fmt is architecturally sound.** It reuses the production lexer (`lex_with_trivia_in`) and parser — no second grammar; author-choice trivia is an explicit, tested, 46-line module; the re-parse + AST-equal safety gate (with a precisely-scoped relaxation for extension-formatted tier bodies) is the right invariant; config discovery is one `FmtConfig::discover` shared by CLI and editor.
- **noeta-db is real salsa, not a cache veneer, on the editor path**: in-place `set_text` with a documented no-disk-IO keystroke path, deliberate backdating design in `workspace_text_tiers` (value-comparable so unrelated edits stay memoized), the always-replace `Update` trade-off documented and miri-gated, and a test pinning salsa-link ≡ batch-loader byte identity (`module_graph_reproduces_the_standalone_loader`). The dumbest sound thing (fresh db per MCP/check call) was chosen knowingly and labeled.
- **Layering holds where it matters**: noeta-ide has no VM dependency (its only `noeta-compiler` use is the hotswap differ for impact analysis); the DAP touches the VM solely through the `Debugger` hook; the two "reach around the IDE" consumers (DAP, MCP execute/debug) do so with written rationale in their Cargo.tomls.
- **Comment culture.** Nearly every trade-off this audit checked had its rationale written at the decision site — several would-be findings dissolved on reading the adjacent comment.

---

# Decomposition sketch for `crates/noeta-cli/src/lib.rs`

Keep `run_cli` + the clap types in `lib.rs`; move everything else, mechanically:

```
crates/noeta-cli/src/
  lib.rs            — Cli/Command enums, run_cli dispatch, unknown-subcommand
                      recovery chain (bare-file / tier / external / compose)   (~700 lines)
  context.rs        — the shared prologue plumbing:
                        ProjectContext { entry, edition, deps, providers, active_tiers }
                        (wraps compose::maybe_delegate, target_gate, load_linked,
                         resolve_providers)                                    (finding 3)
  output.rs         — emit_diagnostics{,_mapped}, emit_trace, plural, human_bytes
  cmd/run.rs        — cmd_run, run_program, execute_real_host, run_module_real_host,
                      p2p_app_namespace, program_args
  cmd/check.rs      — cmd_check, CheckReport, noe_files
  cmd/test.rs       — cmd_test + TestCase/TestOutcome + run_tests/run_one_test + attr helpers
  cmd/bench.rs      — cmd_bench + BenchOutcome + calibration/baseline persistence
  cmd/doc.rs        — cmd_doc, cmd_doc_api, cmd_doc_package (docgen.rs already split)
  cmd/build.rs      — cmd_build, cmd_dump, emit_bundle/emit_exe/emit_wasm/emit_serve
  cmd/native.rs     — emit_native, aot_ring_features, resolve_aot_runtime, link_native,
                      workspace_target_dir, resolve_*_runner                    (~750 lines)
  cmd/serve.rs      — serve_parallel_impl/_hot, run_worker/_hot, run_program_hot,
                      CliCommandCtx, ext_command_clap/dispatch, install_shutdown_handler
  cmd/fmt.rs        — cmd_fmt, cmd_fmt_stdin, fmt_text_tiers+fmt_tier_formatters
                      (merged per finding 10), collect_noe_files, atomic_write
  cmd/repl.rs       — cmd_repl, repl_bootstrap, repl_step, repl_meta, repl_type,
                      eval_entry, check_entry_gate, unclosed_delimiters
  cmd/pm.rs         — cmd_add/update/publish/audit/key/claim/scope + trust/claim helpers
  cmd/servers.rs    — cmd_lsp/cmd_dap/cmd_mcp/cmd_profile/cmd_cache (one-liners)
```

Everything above is `pub(crate)` moves plus the `context.rs` extraction; no behavior change, and `compose.rs`/`watch.rs`/`docgen.rs` already establish the module pattern. The one seam worth doing *while* moving: `cmd/native.rs` and `cmd/serve.rs` are candidates for eventual promotion out of the CLI crate entirely (into `noeta-runner`-adjacent crates), but that can wait until something else needs them.
