# Noeta — Full Architectural Audit (local main @ `af782ab7`)

**Date:** 2026-07-16
**Method:** Six parallel deep-read audits over a detached read-only checkout of local main (`.claude/worktrees/audit-main`), one per area: VM & runtime core, extension/native-ABI surface (extra-hard look, per request), compiler front-end, tooling layer, package manager & distribution, cross-cutting/workspace. Every finding cites `file:line` evidence against that checkout; several were verified by compiling probes or `cargo check`-ing feature shapes. Findings that would have contradicted a documented intent comment were either dropped or explicitly marked as challenging a documented decision. Detailed per-area reports accompany this synthesis (`audit-1` … `audit-6`).

**Scale:** 52 crates, ~182k lines of Rust.

---

## Executive summary

The architecture is in **substantially better shape than a 182k-line single-author language implementation has any right to be**. The load-bearing decisions — the differential-oracle spine (VM ↔ IR-interpreter, byte-identical, 617-file corpus, zero skips), the strict crate DAG, semantics-once-in-`noeta-stdlib`, typed diagnostics with one renderer, the `unsafe` quarantine, and the intent-comment culture — all *verify* under adversarial reading. Auditors repeatedly reported that would-be findings dissolved on reading the adjacent comment.

The real problems cluster into five themes, in priority order:

1. **The extension surface has a genuine identity bug and several half-finished threading seams** — the most severe cluster, exactly where you asked for the extra-hard look.
2. **"Assemble the compile pipeline" exists in ~5 divergent copies** — DAP, profiler, MCP-debug, and `repl --load` silently can't see dependency packages or tiers; a dozen CLI verbs hand-thread editions.
3. **Two user-visible correctness bugs outside the oracle's reach**: `noeta fmt` destroys surface sugar (`#{…}`, `+=`, `~=`, `??=`), and `noeta.lock` doesn't actually pin registry versions.
4. **The god-files are real but the cure is proven and cheap**: the VM split was *already done once* and regrew because nothing ratchets it; the same verbatim-move pattern applies to `noeta-check` and `noeta-cli`, with zero perf risk if the documented constraints are honored.
5. **A crop of perf freebies** where fixing the architecture *is* the optimization (LSP double-checking per keystroke, per-element allocation in `map`/`filter`, dead extension fast-paths, double dependency resolution per CLI invocation).

One meta-finding deserves emphasis: because the comment culture is the codebase's actual architecture documentation, the **stale intent comments** found at load-bearing seams (noeta-eval still narrates itself as the deleted M0 tree-walker; `noeta-value/Cargo.toml` claims stdlib has no internal deps; DAP claims "same pipeline as run") are disproportionately harmful — both agents and humans calibrate on them.

---

## Tier 1 — Correctness bugs to fix first (all are cheap relative to severity)

### 1.1 `noeta fmt` silently rewrites surface syntax *(compiler F1 — HIGH, verified by execution)*
The parser desugars `#{a,b}` → `[a,b].to_set()`, `x += 1` → `x = x + 1` (also `~=`, `??=`) with no AST marker, so the formatter prints the desugared form: **running `noeta fmt` permanently rewrites the author's code**, and the fmt safety gate (re-parse → same AST) is structurally blind to it — the corpus asserts safety+idempotence, never input preservation. Evidence: `noeta-parser/src/lib.rs:1555,619-627`; probe-executed proof in the detail report. *Fix now:* add the two missing resugaring recoveries (the `if_then_else_form` technique already exists at `noeta-fmt/src/print.rs:1806`) + a corpus self-format assertion. *Fix right (per `ARCHITECTURE.md:118`'s own "surface sugar stays in the AST" principle):* real AST nodes, desugar in `noeta-ir::lower`.

### 1.2 `noeta.lock` does not pin registry version selection *(pm F1 — HIGH)*
`Walker::solve` (`noeta-pm/src/graph.rs:722-784`) always re-solves against the live index; the lock's `version` field is written but never read. Three separate doc comments (registry.rs:1190, lib.rs:5, manifest.rs:198) describe lock-pin behavior that doesn't exist, and the yank model documented in the registry repo's PROTOCOL.md is thereby broken (a locked-but-yanked version silently resolves to a *different* version). Consequences: no reproducible builds from a committed lock, mandatory network per build, silent floating upgrades. *Fix:* lock-satisfaction fast path in `solve` before touching the index — also removes the network round-trips.

### 1.3 Extern-type runtime identity is the short name *(extension F1 — HIGH)*
The docs promise qualified identity (`noeta-native/src/registry.rs:487-492`); the runtime keys every dispatch/route-cache/`is`-test on the **short** `type_name()` and `find_type` returns the first match across units (`registry.rs:1243-48`, `noeta-vm/src/lib.rs:4668`, `methods.rs:40`). A third-party `acme/metrics` `Counter` silently dispatches into `std.metrics`' Counter. `validate()` never checks type names at all. *Fix step 1 (zero cost):* reject duplicate short names in `validate()` — turns silent mis-dispatch into a startup panic. *Step 2:* qualified `type_name()`, migrate the caches.

### 1.4 `Session::hot_swap` checks against the wrong registry *(extension F2 — HIGH)*
`Builder::load` threads the per-session registry end-to-end (IR5), but `hot_swap` calls plain `check_all` (`noeta-embed/src/lib.rs:466-471`) — the `Session` struct doesn't even retain the registry. Hot-swapping code that uses session-private extensions is wrongly rejected (or checked against wrong signatures). ~10-line fix, and it sits on the crate's canonical use case.

### 1.5 Debugger, profiler, MCP-debug, and `repl --load` can't see dependency packages or tiers *(tooling F1 + cross F1 — HIGH)*
`noeta-dap/src/session.rs:65`, `noeta-prof/src/session.rs:65`, `noeta-mcp/src/debug.rs:472`, `noeta-cli/src/lib.rs:5745` all load via bare `noeta_loader::load` — no `resolve_graph`, no `load_with_deps`, no tier activation — while both files' doc comments claim "the *same* production pipeline `noeta run` drives." A program using `use <dep>.…` runs, checks, and resolves in the editor, then fails the moment you set a breakpoint or profile it. This is the direct cost of pipeline assembly existing in ~5 places. *Fix:* one `compile_project(entry, opts)` in `noeta-runner` (mostly exists as `compile_whole_file`); route DAP → MCP-debug → REPL through it.

### 1.6 Composed-toolchain cache key misses path-dep native source *(pm F4 — MEDIUM but nasty)*
`compose_key` (`noeta-cli/src/compose.rs:374-421`) covers Cargo.toml bytes but not source content; its justifying comment ("a path dep's tree hash changes on edit and feeds fresh dirs") is factually wrong for path sources — their `dir` is fixed. Editing a path-dep native package (the exact `packages/para-p2p` dev workflow) serves a **stale extension** until Cargo.toml changes. *Fix:* fold the already-computed `content_hash` into the key.

Also in this tier as one-liners: **`noeta publish` ignores `[registries]` per-scope routing** (pm F5 — a private package can be published to the public registry; route through `open_source`), and **opening a file in the editor can rewrite `noeta.lock`** (pm F6 — `resolve_graph` has a hidden write side effect and the IDE calls it; split read from write).

---

## Tier 2 — The god-classes: proven decomposition, no perf regression

Your instinct about the VM is right, and the good news is the repo has already proven the safe pattern.

### 2.1 `noeta-vm/src/lib.rs` (10,685 lines) regrew past its own completed split *(vm F1)*
`plans/code-quality/split-vm-lib.md` records the split as done at 5,729 LOC with a ~1,500-LOC/module goal; five later arcs (tier-1 glue, JIT engine mgmt, hot-swap, isolates, entry-point family) all defaulted back into lib.rs. The extraction pattern (verbatim `impl Vm` moves — `methods.rs`, `scheduler.rs`, `values.rs` were moved with the differential byte-identical) is zero-risk. **The full module-by-module sketch is in the detail report** (`hooks/backend/tier1/lifecycle/dispatch/hotswap/calls` + tests split; lib.rs → ~600 lines). Two constraints to preserve, both already documented in-repo: the `dispatch` match stays one function (jump-table codegen — assessed and declined to split, correctly), and per-call-path extractions stay `#[inline]` and get re-benched against `benches/vm.rs` (the protocol the `call_builtin_method` extraction established at ±0). **Add a ratchet** (CI line-count check or a standing plans note) or it regrows a third time.

Related, same crate:
- **`Vm` struct is a ~66-field god-bag** (vm F3) — group into `tier1`/`sched`/`isolates`/`persist`/`out` sub-structs; identical machine code (flattened offsets), better borrow-splitting, and it collapses ~18 `#[cfg(jit)]` attributes into one.
- **`SessionState` mirrors 16 Vm fields across 4 hand-maintained sites** (vm F4) — becomes `vm.persist`, deleting the silently-drops-state failure mode.
- **`noeta-value/src/lib.rs` (4.3k) and `noeta-jit/src/lib.rs` (4.2k)** are the next two god-files, with equally mechanical splits (vm F8, F11).

### 2.2 `noeta-check/src/lib.rs` (8,115 lines; 45-field `Checker`) *(compiler F2)*
Same disease, same cure: extraction already started (`tiers.rs`, `stdlib.rs`, `packed.rs`, `attributes.rs`), then four arcs landed in lib.rs anyway. The detail report has a full cut-line map (env/sites/prelude/relevance/collect/decls/traits/effects/`expr/{core,ops,calls,member,patterns}`/subst) with one deliberate non-split: the bidirectional `check ↔ synth` core stays one file — the mutual recursion *is* the algorithm. Group `Checker`'s fields into `Symbols`/`Imports`/`Coloring`/`Config` during the move so each module's borrow surface is explicit.

### 2.3 `noeta-cli/src/lib.rs` (6,336 lines) *(tooling F2)*
Not "thin glue" as its header claims: it embeds the serve worker pool, the entire AOT link driver, the test runner, the bench runner, and the REPL. Full `cmd/*` decomposition sketch in the detail report; purely mechanical, `compose.rs`/`watch.rs`/`docgen.rs` already establish the pattern. Do the **tier-prologue dedup** (test/bench/doc triplicate the same 45-line sequence — tooling F3) and the **`ProjectContext` extraction** while moving.

---

## Tier 3 — The extension surface (the extra-hard look): systemic pattern

Beyond the two Tier-1 bugs, the extension findings share one shape: **every mechanism on this surface was built right and then threaded 80% of the way**. The registration/dispatch flow map is in `audit-2`.

- **Dead fast paths** (ext F3): `static_dispatch_ctx` matches bare `"cell"`/`"reactive"` but module identities have been root-qualified (`"std.cell"`) since the namespaced-types arc — the H5 monomorphized route silently never fires, and no test asserts it does. The type-method twin hard-binds `Cell`/`Signal`/… short names ahead of the instance registry. Fixing this *restores* a measured optimization.
- **Global-vs-session registry duality** (ext F6, cross F5, tooling F7): the per-session threading (IR1-IR5) coexists with ~60 documented facade fallbacks — fine as a recorded decision — but the salsa `ast`/`workspace_text_tiers` queries read the global registry as a **hidden non-salsa input** (`noeta-db/src/lib.rs:222-228`), a correctness landmine the moment an embedder pairs a `LangDatabase` with `Builder::with_extensions`. Make the tier-name set an explicit salsa input; funnel remaining global reads through one greppable accessor.
- **Unenforceable author contracts** (ext F4): `key_capable` total-order promises, `arena_getter` equivalence, `type_name` matching, `ExtState` borrow discipline, `SpawnBox::clone` → `unreachable!` — each compiles clean and corrupts or aborts at runtime. Add debug-gated verification + the `SpawnBox` type-level fix.
- **`namespace:` defaults to `"std"`** (ext F5): a third-party type that forgets the field squats the reserved std namespace until publish-time lint. Make it mandatory or validate at assembly.
- **`validate()` gaps** (ext F13): types, tiers, attributes, formatters, capabilities, and commands are all first-wins-silent; the registry's own philosophy is "a mis-assembled binary must not start."
- **`Host` is solved for consumers, unsolved for implementers** (ext F7, cross F9): capability traits + blanket impl = good ISP, but a custom host must write ~70 methods and every new capability breaks every out-of-tree host. Ship a delegating `HostOverlay<B: Host>`. The Sandbox/Wasi/Browser trio also copy-paste `Rng`/`Clock`/`Ids` impls line-identically — extract component structs next time a capability lands.
- **`NativeCtx` at 39 methods** (ext F9, cross F12): the sub-trait accessor pattern (`task_context()` et al.) exists *because* of this trait's history — finish it for the packed-buffer and arena groups; keep the slot/call core.
- **Two marshalling generations + ~1,000 lines of Ring-1 semantics inside "the ABI crate"** (ext F8): mechanical `ring1` module split; fold `Arg`/`Output` into `NativeValue`/`NativeOut` later.
- **No ABI version constant anywhere** (ext F10): sound today (source-level unification via the `[patch]` mechanism), but add `ABI_VERSION` + write down the semantic-contract list before any registry hosts native packages.
- **`Box::leak` per session-load** (ext F14, cross F6): intern assembled registries by unit-set (~20 lines) so the canonical game-engine consumer doesn't leak unboundedly.
- **Dispatch boilerplate** (ext F11): the signature table, checker mapping, and hand extraction are triple-maintained truth across ~4,400 lines of std registry; pilot a declarative macro on one new module.

---

## Tier 4 — Threaded-state & lifecycle hygiene (the "variables threaded through the entire lifecycle" ask)

- **Editions residue** (compiler F4, F8; pm F11/cross F4; tooling F11): the loader **lexes dependency sources under `Edition::DEFAULT`** while parsing per-package (`noeta-loader/src/lib.rs:487`) — the one leg where the arc's "plumbing done before the first divergent edition" claim is false; `DepPackage.edition` round-trips typed→String→`unwrap_or_default()` (silent-default, contradicting noeta-edition's own hard-error policy, on a stale premise since `noeta-edition` became a leaf crate); REPL prompt fragments lex/parse edition-blind; and ~12 CLI sites hand-thread `linked.editions` where a `check_linked(&Linked)` entry would make forgetting impossible. All small; worth a single follow-up slice on the editions arc.
- **Site-map triple bookkeeping** (compiler F6): `SiteMaps` → `Sites` → `LoweringSites` with the identical projection hand-copied at seven call sites across four crates; one `From` impl fixes it. Note the drift risk is precisely where the differential oracle is blind (perf-only maps).
- **`SourceId` order as an implicit cross-crate contract** (pm F8 + cross F7, found independently twice): the cache-hit path hand-reconstructs the loader's id-assignment order; a loader ordering change silently mis-attributes spans on warm cache hits only. Export `workspace_sources()` from the loader.
- **Entry-point combinatorics** (compiler F9, vm F14): `CheckOptions` exists specifically to prevent `_with_x_and_y` families — and `noeta-compiler` (`compile_with_sites_session_with_registry(…, false, false)`), `noeta-ir::lower`, and `VmBackend` (~15 `run_module_*` variants) all grew the family anyway. `CompileOptions`/`LowerOptions`/`RunOptions` with thin presets.
- **Stringly-typed pm errors** (pm F7 + cross F3): 148 `Result<_, String>`s on a library surface; the IDE consequently swallows *every* resolution failure into "no dependencies" (`noeta-ide/src/lib.rs:476`), surfacing trust violations as spurious unknown-import diagnostics. One `PmError { kind, message }` enum, migrate the `Index` trait first.
- **`use`-classification encoded 3×** (compiler F3): `classify_use` is the declared SSOT but the bytecode compiler and eval both re-derive it privately — the one semantics fork *not* per-spelling covered by the oracle. Mechanical substitution.
- **REPL trailing-expr desugar copy-pasted** across both session backends (compiler F10).

---

## Tier 5 — Perf freebies (architecture fixes that are also wins)

| Fix | Where | Expected effect |
|---|---|---|
| Point LSP diagnostics at `linked_checked_ide` (one-liner) | tooling F5 | Halves per-keystroke checker cost — currently 2 full workspace checks per edit, contradicting the db's own documented single-run design |
| Share salsa inputs across open documents (workspace-per-directory) | tooling F6 | Editor cost is currently O(open-docs × dir-size) per keystroke; stacks multiplicatively with F5 |
| Pool the re-entrant run context in `call_value` | vm F5 | `xs.map(f)` currently does ~5 allocs/element, incl. a 64KB reserve and two O(module) cache vecs per call — the repo's own `ctx_table_pool` pattern applies |
| Inline-cache enum methods & operator overloads; intern method-table keys | vm F7 | Removes 2 String allocs per dynamic dispatch on enum/operator paths |
| Lock fast path + thread `ResolvedGraph` from `maybe_delegate`; stop re-fetching releases in `materialize`; reuse `Fetched.content_hash` | pm F1/F2 | Kills the 2× graph resolution, 2× registry fetch, and 4-6× git-tree hashing per `noeta run` |
| Restore the qualified-name fast routes | ext F3 | Re-enables the measured H5 optimization |
| `noeta check` directory mode: link siblings once (or ride salsa) | tooling F4 | Currently quadratic in directory size |
| Merge fmt's mirrored tier scans | tooling F10 | Halves the pre-format project scan |

Plus the front-end allocation tax (compiler F12): `Env::lookup` clones a Box-recursive `Type` per identifier reference on the per-keystroke path — return `&Type` first, intern only if profiles demand.

---

## Tier 6 — Hygiene sweep (cheap, high leverage given the comment culture)

- **Stale intent comments at load-bearing seams** (compiler F5, vm F13, cross F11): noeta-eval's headers still describe the deleted M0 tree-walker (and `TreeWalkBackend` still carries the name); `noeta-value/Cargo.toml` claims "stdlib has no internal deps"; DAP/prof claim "same pipeline as run"; a JIT SAFETY comment describes a 3-arg signature for a 7-arg transmute; `ARCHITECTURE.md` still lists p2p in the Host union and claims the backend-mirror debt is "tracked in plans/" (no such plan exists — write it, per vm F9). One documentation sweep.
- **`lang:` vs `noeta:` stderr prefix** (cross F8): 128 sites still emit the pre-rename brand.
- **`DiagnosticCode` triple list** (compiler F13): macro or count-assertion so `ALL` can't silently miss `from_code` entries.
- **Terminator duality** (compiler F7): two coexisting newline-termination algorithms with different depth semantics; converge on the offsets-based one behind a lexer-snapshot differential.
- **CI**: `cargo check` the ~5 lean feature shapes (`vm/aot`, `runtime no-default`, `para-p2p/ring-p2p`…) that currently only compile inside slow e2e jobs or never (cross F10). `LocalIndex::publish` lossy rewrite (pm F10). Registry wire schema golden fixtures shared with the Worker repo (pm F9). `[targets.*.dependencies]` is parsed+documented but never resolved by any production path — wire it or hard-error (pm F3). Split `cli/tests/cli.rs` into modules within one binary (tooling F12).

---

## What's already good (preserve these on purpose)

- **The differential-oracle spine** — two independent execution stacks meeting only at `Backend`/`RunResult`, JIT/session/leak variants, zero-skip corpus. The strongest duplication-control regime any auditor had seen at this scale; the intentional VM↔eval mirror should *stay* (only its plans-ledger entry is missing).
- **`noeta-jit-abi`, `noeta-reactive-abi`, `noeta-edition`, `noeta-backend`** — every seam crate has a paid-for, documented reason to exist.
- **Host capability ISP for consumers**; the `VmCtx` slot-table ownership model; `MapKey`; NaN-box encapsulation with the one deliberate bit-contract; `Op` size enforced by test; the single `error()`/`Abort` VM error chokepoint.
- **Cache-key discipline in noeta-cache** (domain-tagged, order-independent, each ingredient pinned by a regression test naming its bug) and one `.noeb` envelope shared by cache/build/exe/wasm.
- **Trust evaluation as one pure tested function**; hermetic in-process CA/CT/Rekor fixtures; top-down authority for `[trust]`.
- **noeta-lsp as a genuinely thin adapter; noeta-fmt's re-parse safety gate; noeta-db as real salsa** on the editor path.
- **The intent-comment culture** — the single biggest reason this audit could distinguish rot from design. Findings 1.1–1.6 notwithstanding, most "suspicious" code had its justification within 20 lines.

---

## Suggested sequencing

1. **Bug week** (each independent, small): fmt resugaring (1.1), lock fast path (1.2), `validate()` short-name rejection (1.3 step 1), `hot_swap` registry (1.4), compose key (1.6), publish routing, IDE lock-write split, LSP diagnostics one-liner.
2. **One-pipeline slice**: `compile_project` in noeta-runner; migrate DAP → prof → MCP-debug → REPL → `load_linked`; add `check_linked`. Kills tooling F1/cross F1/compiler F8 in one arc.
3. **Extension-surface slice**: qualified type identity end-to-end + fast-route restoration + `validate()` completion + salsa ExtEnv input + namespace mandatory. (This is the highest-value *architectural* arc; it de-risks the whole third-party-native story before the registry hosts native packages.)
4. **God-file arcs** (mechanical, one crate each, oracle-gated per move, bench-gated where `#[inline]` matters): vm → check → cli → value → jit. Add the line-count ratchet with the first one.
5. **Hygiene sweep** as a standing low-effort track: stale comments, `lang:` prefix, options structs, CI feature shapes.

Per-area detail reports: `audit-1-vm-runtime.md`, `audit-2-extension-surface.md`, `audit-3-compiler-frontend.md`, `audit-4-tooling.md`, `audit-5-pm-distribution.md`, `audit-6-cross-cutting.md`.

The read-only checkout used for all line references remains at `.claude/worktrees/audit-main` (detached @ `af782ab7`); remove with `git worktree remove` when done.
