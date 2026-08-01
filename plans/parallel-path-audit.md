# The parallel-path audit — where the next four bugs live

Status: **in progress on `audit/parallel-paths`** since 2026-08-01 — the rows are being built, not just proposed. Each closed row carries a note in its own section saying what was built and what the row got wrong; this header says only which are done. Everything below the header is the original reading pass, kept as written so the *predictions* can be scored against the outcomes.

**Closed:** row 1 (one run tail), row 3 (provenance is one value), row 7 (one jump-target answer), row 10 (one hot install). **In flight:** rows 5, 6, 9, 11, 13. **Open:** rows 2, 4, 8, 12, 14.

Two corrections the work has already made to the reading pass, both worth carrying into the next audit: it **undercounts copies** (row 7 said four sites, there were five; row 1 said seven tails, there were eight — plus the oracle, a ninth; row 3 said four bad `CheckOptions` literals, there were six, two of them inside the crate the row was about), and it **under-sizes fixes that are structurally right** (row 3's "~40 lines in `noeta-check`, ~11 call sites" became a value object threaded through the loader, because constructors alone still let a caller pass two of three arguments). A reading pass sees the sites it greps for and prices the patch it can picture. Neither error is in the safe direction.

The compile/swap arc closed a bug class by deleting a copy. `compile_to_mc` and `SessionCompiler::extend_impl` were two implementations of one eleven-step sequence, four shipped bugs lived in the delta, and the fix was one `install` plus a `TABLE_POLICIES` census — one function, one list, no second place to forget. The module docs of `crates/noeta-compiler/tests/pipeline_tables.rs` and `crates/noeta-ir/tests/lowerer_field_census.rs` tell that story better than this file can.

This audit went looking for the next instances of that shape *before* they cost four more bugs. It is a reading pass: no production code changed, nothing was built.

Ranking is by **evidence of drift**, not aesthetics. A row earns its place by being wrong *today*, or by having been wrong before, or by having a silent failure mode with nothing forcing the copies together. Rows 1–5 are wrong today. Rows 6–12 are structural risk in seams that have already failed.

Two sections follow the rows: what is **already tracked elsewhere** (so this file does not duplicate a plan that exists), and what was checked and found **fine** — a real result, recorded so the next audit does not re-walk it.

---

## 1. `RunResult.stderr` is written by two of seven run tails, and the wasm oracle forgot it the same way

**CLOSED 2026-08-01** — one `RunTail`, seven (eight) callers, four live defects fixed. The findings below are kept as written; the closing note is at the end of the section.

`std.io`'s `err`/`errln` are *observable program output*, buffered into `RunResult.stderr` (`crates/noeta-backend/src/lib.rs:18-24`, filled at `crates/noeta-vm/src/lifecycle.rs:463`) exactly as `stdout` is. Seven places turn a `(RunResult, trace)` into process output. Seven hand-written copies of one epilogue.

| site | stdout | **stderr** | diagnostics | traceback |
|---|---|---|---|---|
| `crates/noeta-runner/src/lib.rs:88-121` `run_compiled_module` | ✅ | ✅ `:100` | `render_mapped` | ✅ |
| `crates/noeta-cli/src/cmd/run.rs:41-55` | ✅ | ✅ | `render_mapped` | ✅ |
| `crates/noeta-cli/src/lib.rs:1729-1736` `run_declared_tier` | ✅ | **✗** | `render_mapped` | ✅ |
| `crates/noeta-aot-runtime/src/lib.rs:99-111` (`build --native`) | ✅ | **✗** | hand-rolled `render` loop | ✅ |
| `crates/noeta-wasm-runner/src/main.rs:111-126` (`build --wasm`) | ✅ | ✅ *(fixed)* | `render_mapped` | ✅ |
| `crates/noeta-wasm-serve/src/lib.rs:161-183` (`wasi:http` edge) | ✗ on success | **✗** | — | — |
| `crates/noeta-cli/src/cmd/serve.rs:559-567` / `:678-686` (workers) | ✅ | ✅ | **✗** | **✗** — prints `[worker] aborted` |

(The table above is the state the audit found. **Row closed** — see the closing note at the end of
this section; every cell is ✅ now, through one shared value rather than seven agreeing copies.)

`run_compiled_module`'s own doc says it exists so "the CLI (`run`, bundle run) and the standalone runner … both present identical output". **It is the chokepoint, and five of the seven tails do not call it.**

The history is the archetype exactly. The stderr stream landed in `b0606a6ab` (2026-07-24, "CLI-completion slice 1: stderr stream + `std.io` module"). The AOT runtime is `1aa9e5390` (07-07), the wasm runner `ece433f3e` (07-11), the wasm oracle `bc650e3cc` (07-11). A feature threaded through the tail its author was looking at; three older copies of the same tail kept their pre-feature shape.

**The oracle was an eighth copy, written from the same forgotten template.** `crates/noeta-conformance/src/wasm.rs:314-323` composed `expected_stderr` from `native.diagnostics` + `render_trace` and never added `native.stderr`. (Fixed — see the Progress note below.) The module doc frames the equality as "stdout, exit code, and rendered stderr (diagnostics + traceback)" — the *definition* of the comparison excludes the program's own stream. Both sides drop it and they agree.

`tests/conformance/io/streams.noe` exists precisely to pin both streams (`// expect: stderr "warn"`, `"second"`). The eval↔VM and JIT differentials catch it — they compare the whole `RunResult` struct. The wasm oracle ran that exact case, at `0 skipped`, with a vacuous stderr assertion — and, on this machine, was not running at all.

Same file, same shape: `crates/noeta-aot-runtime/src/lib.rs:111` writes `result.exit_code as u8` where the runner writes `u8::try_from(...).unwrap_or(1)` (`crates/noeta-runner/src/lib.rs:120`). **A `--native` binary exiting 256 exits 0.** The AOT tail also never calls `with_live_output`, so a long-running `--native` program buffers everything until exit.

**Failure mode.** A program that writes diagnostics to stderr — the normal shape for a CLI — is silent under `noeta build --native`, under `noeta build --wasm`, at the `wasi:http` edge, and under a declared-tier run. No error, no warning, no failing gate.

**Chokepoint.** Make `run_compiled_module` (plus a `RunTail` value beside it for the surfaces that want structured output rather than process streams) the only thing that renders a run, and call it from all seven. Then add `native.stderr` to the oracle's `expected_stderr`. **Size: ~150 lines moved, plus one line in the oracle.** The oracle line lands first: until it does, the gate meant to protect the wasm tail is the reason the bug survived.

**Progress (2026-07-31).** The oracle line landed, and with the oracle finally *running* (see below) it immediately caught three cases the wasm tail was dropping — `io/streams.noe`, `io/to_string_parity.noe`, `http_server/serve_handler_abort_is_reported.noe`. The wasm runner's tail now writes `result.stderr`, pinned by two tests in `crates/noeta-wasm-runner/tests/runner.rs` that assert the bytes *and* their order relative to the traceback. **Four tails still drop the stream** — `run_declared_tier`, the AOT runtime, `wasm-serve`, and the serve workers' missing diagnostics/traceback — so the row stays open and the chokepoint is unchanged.

That the oracle was blind here for weeks had a second cause worth recording with the first: it was **reporting SKIP** in `scripts/gate.sh`, because `wasmtime` lives in `~/.wasmtime/bin` (where its installer puts it, invisible to a non-interactive shell) and `wasm32-wasip1` was installed for `stable` while the gate pins `+1.97.0`. A parallel-path bug and a disarmed gate are the same failure at different altitudes: the copy drifts, and the thing that would have noticed was not running. The gate now probes the real install dirs, names which prerequisite is missing with the command that fixes it, lists every skipped step in the summary, and fails rather than skips under `NOETA_GATE_REQUIRE_TOOLS` (default-on in CI). The oracle also fails a whole-corpus run that executed zero programs.

A note for whoever does the unification: `run_compiled_module` would **not** drop into the wasm runner as-is. The runner constructs its own `Host`/`Executor` pair (`SandboxHost` vs `WasiHost`) and runs `run_module_debug(…, None)`, whereas `run_compiled_module` owns host selection itself. The reusable piece is the *tail* — the `(RunResult, trace, SourceMap) → process output` half — which is why the row proposes a `RunTail` value beside the function rather than the function alone. Split that out and the wasm runner is a two-line caller; hand it the whole function and it is not a caller at all.

**CLOSED (2026-08-01), `audit/run-tail`.** `noeta_backend::RunTail` is the epilogue, and all seven tails call it — plus an eighth the audit missed, `serve.rs`'s single-process `run_program_hot`.

It went into `noeta-backend`, not "beside `run_compiled_module`" as the row proposed, and the reason is worth recording: `noeta-runner` links the L2 compile front-end, so the AOT runtime staticlib (whose `aot_runtime_does_not_link_the_compiler_frontend` guard exists to keep it out) and the two wasm crates (which have no `noeta-host-real`/tokio) cannot depend on it. `noeta-backend` already owns `RunResult` and `render_trace` and is the common ancestor of every execution surface. The type is `render` → components → `emit()` / `parts()`, so a surface that wants *structured* output — the `wasi:http` edge composes a 500 body, a serve worker labels an abort — reads the parts instead of hand-writing the epilogue again, which is how seven copies happened in the first place. `parts()` destructures `Self` exhaustively, so a **new output component** fails to compile until it is classified onto a stream and given a position in the order, and every structured consumer picks it up with no edit.

Four live defects went with it: the dropped stderr on `run_declared_tier` / the AOT runtime / the `wasi:http` edge (which also dropped stdout on the success path); `result.exit_code as u8` on the AOT path (a `--native` binary exiting 256 exited 0); the AOT host's missing `with_live_output`; and the serve workers' `[worker] aborted`, five words replaced by the diagnostic and the stack.

Two more the row did not name, both found by looking for the copies of a thing it did name:

- **The serve workers had the AOT tail's live-output bug too.** `run_worker`, `run_worker_hot` and `run_program_hot` all built their real host without `with_live_output`, while the single-worker `serve` path — through `run_program` — had it. A server never exits on its own, so a `--parallel` worker buffered every byte it printed for its whole lifetime: `echo` from a request handler appeared *never*, not merely late. One defect, two surfaces, found once.
- **The oracle is a caller now, not a ninth copy.** `crates/noeta-conformance/src/wasm.rs`'s `compare` composed its expected stderr by hand and forgot `native.stderr` — the same omission, in the gate that existed to catch it, so both sides dropped the stream and agreed. Adding `native.stderr` (the 07-31 fix) made it right; building a `RunTail` makes it *unable to drift*, and a future output component reaches the comparison by construction. `exit_code_byte` deleted with it — that was the tail's exit conversion, hand-copied a tenth time.

The tests are per-surface and behavioural, not "did you call the function": stderr bytes, their order relative to the diagnostics and the traceback, and the exit-code conversion, in `crates/noeta-runner/tests/runner.rs`, `crates/noeta-wasm-runner/tests/runner.rs`, `crates/noeta-cli/tests/cli/run_tail.rs`, `crates/noeta-wasm-serve/src/lib.rs`, and `crates/noeta-cli/tests/parallel_serve.rs` (in `hot-e2e.sh`'s census, count bumped to 2). `build_native_matches_a_source_run_byte_for_byte` is the one that mattered most and was the weakest: it compared **stdout only** over a fixture that wrote no stderr and `return`ed silently without `cargo`/`cc` — the only gate on a surface with no oracle at all (row 9), reading green over three defects. It now compares both streams and the exit code, and its skips say what they did not prove. Mutation-verified: restoring the pre-unification AOT epilogue verbatim fails both native tests (`stderr diverged`, and `256 truncated to Some(0) — as u8 is back`).

Row 9 is unchanged by this and is the remaining exposure: `--native` still has no differential oracle, so its correctness rests on those two hand-written programs.

---

## 2. The startup-cache key is a hand-enumerated list of compilation inputs, and it is incomplete today

**Live. Silent wrong answer. Two past shipped bugs of exactly this shape.**

`crates/noeta-cache/src/lib.rs:112` `KeyBuilder::finish` is careful — domain-tagged, length-prefixed, sorted. The *inputs* are not: they are enumerated by hand at `crates/noeta-runner/src/compile.rs:469` `open_startup_cache` and `:570` `key_deps`.

`FrontFacts` (`compile.rs:146`) has five fields; `open_startup_cache` is handed four. **`package_uses` is not folded into the key** — despite the struct's own doc claiming it is "resolved ONCE and shared by the cache key and the loader, so no consumer can pick a divergent subset". It is a real compilation input, threaded to `noeta_loader::load_with_deps` (`:338`) and to `noeta_check::TierContext { uses: … }` (`:300`). It is the per-package `[directives]`/`[tiers]` `@name` → provider table.

Same shape one level down: `DepPackage` has seven public fields (`crates/noeta-loader/src/lib.rs:315-352`); `key_deps` folds five. **`native` and `directives` are not folded.**

**Failure mode.** A program using `@openapi`, with two dependencies exporting that name. Edit the root `noeta.toml` from `[directives] openapi = "para"` to `openapi = "other"`. No `.noe` byte changes; deps, editions, tiers, providers and binary identity are identical ⇒ same key ⇒ cache hit ⇒ **the old provider's generated code runs**. Deleting the binding is worse: that should be E0036, but a hit skips the whole front end, so the error is never reported and the stale expansion runs anyway.

**Past drift.** `2495394f4 fix(cache): key on the entry file, not just the source set` — `noeta run a.noe` then `noeta run b.noe` in one directory ran the same program. And `key_deps`'s own doc (`compile.rs:566-569`): "Before S2 only the root package's edition was keyed, so a dependency-edition change could serve a stale artifact." Two shipped instances, both "an input nobody added to the hand-enumerated key". The tests (`crates/noeta-cache/src/lib.rs:429-561`, `compile.rs:694`) are hand-written "changing X changes the key" assertions — one per input someone remembered, which is the same discipline the key relies on.

**Chokepoint.** A `FrontFacts` / `DepPackage` field census in the style of `lowerer_field_census.rs`: every field is either key material or explicitly declared irrelevant *with a reason*, and a new field fails the test until classified. **Size: ~120 lines of test, plus two or three lines for the live gaps.** `PackageUses` is `HashMap`-backed — folding it in must sort first, or it reintroduces the bug the cross-process determinism gate exists for.

---

## 3. `CheckOptions` is assembled by hand in eleven places behind `..default()`, and two of them are wrong

**Live. Silent. `..default()` is what makes it silent.**

`noeta_check::CheckOptions` (`crates/noeta-check/src/lib.rs:177`) has six fields. Eleven sites build one as a struct literal ending in `..Default::default()`:

`crates/noeta-cli/src/cmd/check.rs:228`, `crates/noeta-cli/src/context.rs:288`, `crates/noeta-cli/src/lib.rs:1718`, `crates/noeta-mcp/src/execute.rs:398`, `crates/noeta-conformance/src/lib.rs:496`, `crates/noeta-db/src/lib.rs:388`, `:408`, `:1229`, `:1265`, `crates/noeta-ide/src/impact.rs:514`, `crates/noeta-runner/src/compile.rs:258`.

**Two omit `package_uses` while setting its neighbours `editions` and `packages`** — `crates/noeta-ide/src/impact.rs:517-520` and `crates/noeta-mcp/src/execute.rs:399-401`. Per the field's own doc (`lib.rs:193-199`), empty means "no package binds any extension `@name`", so an extension directive resolves to nothing and the checker reports **a spurious E0036**. Memory already records E0036 as a footgun; this is a source of it that the author of the file never wrote.

The same two sites also call the context-free `noeta_check::activate_tiers` (`impact.rs:511`, `execute.rs:392`; `crates/noeta-ide/src/lib.rs:2548` additionally hardcodes `&["test"]`) where `cmd/check.rs:262` and `compile.rs:303` call `activate_tiers_with(&ctx)`. `crates/noeta-check/src/tiers.rs:1340` acknowledges the split in prose: "Both are empty for a single-program preview (the IDE/MCP `activate_tiers` path), which then resolves every `@name` ambiently — the behaviour that predates per-package naming."

This is the cleanest example in the audit of a language feature that makes a parallel path *silent*. `..Default::default()` converts "you did not consider this field" from a compile error into a default value. Eleven call sites, one shared struct, and no way for the compiler to ask.

**Chokepoint.** Replace the literals with constructors that take provenance as an argument — `CheckOptions::for_program(editions)` and `CheckOptions::for_workspace(editions, packages, package_uses)` — and make the struct non-exhaustive so a literal cannot be written outside the crate. Then `activate_tiers` and `activate_tiers_with` collapse into one function that takes the context the constructor already carries. **Size: ~40 lines in `noeta-check`, ~11 call sites rewritten, and the two live bugs disappear as a side effect.**

**Done (2026-08-01), and the sizing was wrong in an instructive way.** The constructors alone would have left three arguments a caller can still pass two of, so the triple became a value object: `noeta_edition::Provenance { editions, packages, uses }`, `#[non_exhaustive]`, no `Default`, re-exported as `noeta_check::Provenance`. It lives in `noeta-edition` because that is the lowest crate that can name all three types (`noeta-span` owns `PackageMap`/`PackageUses` and cannot depend upward on editions), which means the **loader** can carry it: `noeta_loader::Linked` now has one `provenance` field instead of three, and so does `noeta_runner::Loaded`. There is no longer a layer at which the three exist apart, so `Linked` cannot be constructed with two of them — the CLI's `linked.package_uses = facts.package_uses;` patch-after-construction is deleted, and `noeta-db` grew one `workspace_provenance` query in place of three separate asks.

`CheckOptions` is `#[non_exhaustive]` with `for_program`/`for_sources`/`for_workspace` + `with_expr_types`/`with_registry`, and `Config::of` destructures it with no rest-pattern, so a new option is a compile error at one site. `TierContext` is deleted and `activate_tiers`/`activate_tiers_with` are one function taking `&Provenance`.

**A fifth instance of the bug, inside `noeta-check` itself, that the audit did not find:** `check_all_session_opts` spelled the `Config` fields out and stopped after `editions`, dropping `packages` and `package_uses` through `..Config::default()`. Every REPL entry, every debug-console fragment and every hot-swap session therefore ran with no package map and no `@name` bindings — no orphan rule, and a spurious E0036 on any project that renames a directive. A sixth: `noeta-mcp`'s debug entry took `loaded.editions` alone and passed it to `check_all_session_with`, so the agent debug tools had the same defect from the other side. Both are structurally unwriteable now.

---

## 4. The package-manager's trust chain: eleven cross-repo byte formats, a manual ritual, and a CI that never runs the tests

**Live (two defects). Silent. The worst failure mode in the audit.**

`crates/noeta-pm` and the Worker at `/home/niklas/Code/noeta-registry` maintain eleven duplicated protocol artifacts — advisory canonical signing bytes, feed-head bytes, feed digest, tier spellings, log record bytes, checkpoint bytes, RFC 6962 Merkle math, attestation bytes, publish-field limits, reserved scopes, the CVSS vocabulary — plus a twelfth nobody counts: `registry/src/semver.ts` is a hand port of the Rust `semver` crate's `VersionReq::matches`, the function `noeta audit` itself calls (`crates/noeta-pm/src/advisory.rs:153`). Its test file records that every expectation "was differentially checked against `semver::VersionReq::matches` itself (a throwaway Rust binary over this exact case list)". The binary is in neither repo. That differential ran once, by hand.

They agree today. `diff -r` of the two fixture directories is byte-identical across all 32 files, and fixture commits pair 1:1 by date four times over. That is exactly why it is worth writing down: the mechanism is a four-step ritual in `crates/noeta-pm/test_data/wire/README.md`, and it has been performed correctly every time so far.

**(a) `noeta audit` degrades a signature failure to a grey line and exits 0.** Any drift in the advisory canonical bytes makes every per-advisory signature fail. `crates/noeta-pm/src/registry.rs:1334` returns `PmError::Trust`; `crates/noeta-cli/src/cmd/pm.rs:1239` prints `not checked — {err}` and falls through to `ExitCode::SUCCESS` (`:1245-1254` exits non-zero only when `advisory_fails > 0`). **A critical advisory against a resolved dependency is never evaluated, and in CI the output is indistinguishable from clean.** The advisory→log binding degrades the same way at `pm.rs:1223`. This is a bug independent of any drift — it is the amplifier that turns a format mismatch into silence.

**(b) The Rust half of the wire-fixture pin never runs in CI or in the gate.** Every relevant module is feature-gated (`registry.rs:2848`, `:3145`, `:3239`, `:3280`; `lib.rs:54`, `:61`), `crates/noeta-pm/Cargo.toml:58-82` declares no `default` feature, and only `noeta-cli` enables them. CI runs `cargo test --workspace --exclude noeta-cli` (`.github/workflows/ci.yml:88`), and the later `cargo test -p noeta-cli` builds `noeta-pm` with the features but runs only `noeta-cli`'s test targets. `scripts/gate.sh:303-330` mirrors this. **The fixture pin, the log and advisory chain verification, the MANIFEST hash test and the attestation golden-literal test all run only when a human types `cargo test -p noeta-pm --all-features`** — step 4 of the ritual.

**(c) The fixture pin is self-referential.** `MANIFEST.sha256` lives *inside* the copied directory and is copied with it, so each repo hashes its own fixtures against its own manifest. Edit a fixture and regenerate the manifest in `lang`, forget to copy: lang green, registry green, protocol diverged. The commit that introduced it (registry `92db98d`) claims "a hash test on each side means neither copy can drift or go stale silently" — true for a local hand-edit, false for the stale-copy case the ritual exists to prevent.

**Live divergence in the publish limits.** `crates/noeta-pm/src/manifest.rs:1643` counts `trimmed.chars().count()`; `registry/src/index.ts:525` counts `.length` (UTF-16 code units). `manifest.rs:1645` uses `char::is_control` (which includes C1, U+0080–U+009F) where `index.ts:1083` uses an ASCII-only regex. A 180-character description with astral-plane characters passes `noeta check` and is 400'd at publish; U+0085 is client-rejected and server-accepted. Loud, but it is the tell that the "MUST match the server" comments are load-bearing and unassisted.

*Verified 2026-08-01, both halves still exactly as described.* The two lines, for whoever fixes it:

```rust
// crates/noeta-pm/src/manifest.rs:1643-1644
|| trimmed.chars().count() > MAX_DESCRIPTION      // Unicode scalar values
|| trimmed.chars().any(|c| c.is_control())        // Cc: U+0000–U+001F and U+007F–U+009F
```
```ts
// registry/src/index.ts:1083
if (trimmed.length === 0 || trimmed.length > MAX_DESCRIPTION || /[\u0000-\u001f\u007f]/.test(trimmed))
//     UTF-16 code units ────────┘                                     ASCII control + DEL only ┘
```

**Adopt the Rust side's semantics, because they are the ones a human means by "characters"** — an emoji is one character, not two — and because the client is where the message is actionable. The registry becomes `[...trimmed].length` (code points, not UTF-16 units) and `/[\u0000-\u001f\u007f-\u009f]/` (the full `Cc` category). Both halves then agree on Unicode rather than on an accident of each language's default string model. Pin it with a boundary-length `publish-request` fixture in the shared wire set — one description at exactly `MAX_DESCRIPTION` code points built from astral-plane characters, and one containing U+0085 — since a limit that only agrees on ASCII is not a limit that was checked.

**Past drift.** Registry `fe9139d`: "reconcile PROTOCOL.md with the implemented wire schema … The spec had drifted from what both sides actually implement." Registry `6407107` documents a *deliberate* divergence window, because lockstep across two repos is painful. And registry `d815220` is the near-miss: the log public key is a raw 32-byte Ed25519 key, not the 44-byte SPKI DER openssl emits by default, and "**the mismatch fails as signatures that never verify rather than as a load error**" — caught only because the author checked production by hand.

**Chokepoints, cheapest first.**
1. `pm.rs:1239` — an unverifiable advisory feed becomes a `✗` and a non-zero exit, not a lowercase note. **~10 lines.** Do this regardless of the rest; it is the difference between loud drift and silent drift.
2. `cargo test -p noeta-pm --features registry-http,provenance --locked` in `ci.yml` and `gate.sh`. **~4 lines**, turning roughly 40% of the crate's test surface from dead to live.
3. Embed the manifest's own SHA-256 as a source constant on each side, so an un-propagated regeneration fails the *other* repo's build. **~20 lines across two repos.**
4. Move the publish limits into the fixture set as a boundary-length `publish-request` fixture and align the counting semantics. **~30 lines.**
5. Commit the throwaway semver differential as `crates/noeta-pm/examples/semver_vectors.rs` emitting the case list as JSON, consumed by the TS test. **~50 lines.**

---

## 5. `noeta check`, the LSP and MCP give three different answers to "is this project clean"

**Live. Silent. The exact failure `noeta check`'s own doc says it closed, reopened one layer up.**

| surface | assembly | per-file entries | per-tier sweep |
|---|---|---|---|
| `noeta check` | `crates/noeta-cli/src/cmd/check.rs` — own walk + `noeta_pm::graph`, `code_tiers_in:246`, `activate_tiers_with:262` | every `.noe` | **yes** |
| LSP diagnostics | `crates/noeta-ide/src/lib.rs:470` → `noeta_db::linked_checked_ide_from` (`crates/noeta-db/src/lib.rs:1257`) | open docs only | **no** |
| MCP `check` | `crates/noeta-mcp/src/lib.rs:1270` `run_check` → `linked_checked` (single workspace entry) | **no** | **no** |

`code_tiers_in` has exactly one production caller — `cmd/check.rs:246`. And `crates/noeta-check/src/lib.rs:2340-2345` states that the checker "validate[s] only the tier name … and do[es] not type-check the (stripped) items."

**Failure mode.** A type error inside a `@test { … }` block is invisible in the editor and to MCP, and reported by `noeta check`. `cmd_check`'s doc comment (`cmd/check.rs:36-41`) describes closing precisely this for the CLI — "`noeta check .` said '0 errors' about a file `noeta test` could not compile". `crates/noeta-ide/src/lib.rs:506-508` names it from the other side: "the editor shows the file as clean while `noeta check` fails on it, which is exactly the compiler/editor disagreement this arc exists to end." Both statements are true, and neither reaches the tier bodies.

**Chokepoint.** One `project_check(root, options) -> Diagnostics` in `noeta-ide` (which already owns the `DocumentStore` all three surfaces share), driving the file walk, the tier activation and the per-tier sweep once. `noeta check` becomes its printer; the LSP calls it for workspace diagnostics; MCP calls it for `check`. **Size: ~200 lines moved out of `cmd/check.rs`, three thin callers.** Row 3's constructor lands first — the three surfaces cannot share a function while they disagree about what `CheckOptions` to build.

---

## 6. `Sites` has thirty-five fields, no census, and one hand-maintained claim about all of them

**No live defect. Sits inside the seam that just cost four bugs, on the half `TABLE_POLICIES` does not cover.**

`TABLE_POLICIES` (`crates/noeta-compiler/src/lib.rs:465-505`) classifies every field of `ModuleCompiler` and `SessionCompiler`, machine-checked by `crates/noeta-compiler/tests/pipeline_tables.rs` reading the source text. That is the fix, and it is good.

`noeta_check::Sites` is the *input* to that pipeline and is not covered by it. Thirty-five public fields (`crates/noeta-check/src/sites.rs:19-289`), each a span-keyed lowering hint the checker computes and `install` consumes. Two per-field obligations ride on it, both stated in prose and enforced by nothing:

1. `SessionCompiler::absorb_type_args` (`crates/noeta-compiler/src/lib.rs:691`) remaps incoming type-argument table indices into session space, and its doc at `:683-686` asserts: "`hidden_arg_sites` is the only `Sites` field that carries a type-arg TABLE index." The `u32`s in `forwarded_slot_sites`, `dynamic_construction_sites` and `self_type_arg_sites` are called out as *not* table indices. That is a judgement about thirty-five fields, restated as a comment, checked by no test.
2. The lowerer census classifies the whole bundle as `Kind::CheckerSites`, "whole-program by construction". Right for the bundle; silent about a *new* field whose population depends on seeing the whole program.

**Failure mode.** Add a thirty-sixth field carrying a type-arg table index — the natural shape for any future call-site-typed feature, since three such fields already exist. A cold compile is fine (empty compiler, indices unchanged). A REPL entry or a hot swap installs it with the *incoming* index against the *merged* table: silently wrong type arguments, which is bug 3 of the original four ("the type-argument table replaced where it had to be merged") reintroduced through the door the fix did not close.

**Chokepoint.** A `SITE_POLICIES` half of `pipeline_tables.rs`: one row per `Sites` field, classified `SpanKeyed` (needs no remap), `TableIndexed` (must be remapped by `absorb_type_args`), or `Content` — parsing the field list out of `sites.rs` the way the existing gate parses `lib.rs`. **Size: ~90 lines of test plus 35 one-line rows.** The rows are the point: writing them is where someone finds out whether the claim at `:686` is still true.

---

## 7. "Which op carries a jump target" is written out four times, twice with a silently-wrong catch-all

**✅ CLOSED** on `audit/op-jump-targets`. The set is now `Op::for_each_jump_pc`/`_mut` in `noeta-bytecode` — one `macro_rules!` arm list, no `_` catch-all, so a new `Op` variant is a compile error there; the ten fields are declared `JumpPc` (a `u32` alias) so the declaration itself says which indices are code. Every site calls it, and the JIT's `succ_all`/`analysis_succ` — the same function written twice with different coverage — collapsed into one `tier0_succ`. `crates/noeta-bytecode/tests/op_jump_pc_census.rs` classifies every `u32`-shaped field of `Op` and fails until a new one is declared a destination or declared not to be. The count of ten was right; the count of sites was five — `regalloc.rs`'s `op_facts` repeated the answer too (safely, being exhaustive over variants).

**No live defect — all four lists currently agree. Nothing forces them to.**

Exactly ten `Op` variants carry a code index (`fallback: u32` ×1 at `crates/noeta-bytecode/src/lib.rs:821`, `fail: u32` ×5 at `:1235-1262`, `target: u32` ×4 at `:1391-1410`). That set is written by hand in four places:

| site | ops | catch-all | what a missed variant does |
|---|---|---|---|
| `crates/noeta-compiler/src/lib.rs:3128` `patch_jump` | 10 | `_ => unreachable!()` | panics — **loud, fine** |
| `crates/noeta-compiler/src/regalloc.rs:55` `for_each_target_mut` | 10 | `_ => {}` | the LICM rebuild does not remap it ⇒ **jump to the wrong index, silent miscompile** |
| `crates/noeta-jit/src/plan.rs:302` `succ_all` | 10 + `Return`/`Halt` | `_ => {}` | missing CFG edge ⇒ under-approximated liveness ⇒ **unsound spill omission in native code** |
| `crates/noeta-jit/src/analysis.rs:128` `analysis_succ` | 4 | falls through | safe *only because* the arithmetic whitelist never sees a `Match*`; if `is_fast_op` grows one, the `fail` edge is silently dropped |

`regalloc.rs:53` documents its list as "the same set `patch_jump` handles". `plan.rs:23-26` documents its list as "the same set the compiler's `patch_jump`/`for_each_target_mut` handle" and names the hazard outright: "A missed edge would under-approximate liveness (an unsound spill omission)."

Two files each state an invariant they cannot check. That is a chokepoint that was described and never built — the highest-yield comment shape in the brief, here in its purest form.

**Chokepoint.** An inherent `Op::for_each_code_index(&mut self, f)` in `noeta-bytecode`, written once with no catch-all, plus a source-text census asserting that the count of code-index fields in the `Op` declaration equals the count the method handles. All four sites call it; `analysis_succ` keeps its whitelist but derives "does this op branch at all" from the shared answer. **Size: ~60 lines of method, ~50 of test; the four call sites shrink.**

---

## 8. The loader's text-tier lexing has a salsa twin, ~80 lines, comment-linked and untested

**No live defect found. Maximally silent failure mode, in the seam between "what the compiler sees" and "what the editor sees".**

`crates/noeta-db/src/lib.rs:717` `workspace_renamed_text_tiers` is documented as "the salsa twin of the loader's `renamed_text_tier_locals`", with "Mirrors the loader's `declared_by_segment`" (`:723`) and "exactly as the loader lines them up" (`:713`); `:765` `source_text_tiers` is "The salsa twin of the loader's per-package re-lex"; `:788` says "Matches the loader's `union` in `lex_program`." The original is `crates/noeta-loader/src/lib.rs:1522` `lex_program`.

Four "twin"/"mirrors" comments, ~80 lines of resolution logic, **no parity test**. This is the same shape as `plans/backend-mirror.md`'s entries, minus the policy: the backend mirror requires a corpus case that would catch divergence, and this twin has none.

**Failure mode.** A `@name { … }` body lexes as verbatim text on one path and as *code* on the other: phantom parse errors in the LSP on a file that compiles clean, or prose bleeding into the code path in the editor while the compiler treats it as text. Nothing surfaces as an error on the path that is right.

Counting the two grammar generators (`noeta grammar tree-sitter` → `project-tiers.json`, and `generated-tiers.tmLanguage.json`), the same "which `@name`s lex verbatim" set is derived in four places. The twin was created by `30136f1ed feat(ide): per-package lexing of renamed text tiers in the editor (3g)` — memory records the remaining lexer half of that item as open.

**Chokepoint.** Lift the resolution — manifest bindings + per-package declarations → the set of verbatim-lexing `@name`s — into one pure function in `noeta-loader` that takes its inputs rather than reading the filesystem, and have the salsa query call it. **Size: ~80 lines lifted, ~30 for the query wrapper.** Failing that, the backend-mirror policy applies as-is: a corpus case that would catch divergence, plus a parity test over a package with a renamed text tier.

---

## 9. `noeta build --native` is the one execution surface with no differential oracle

**No live defect proven — but see row 1. This is *why* row 1 survived.**

`crates/noeta-conformance/src/` holds `differential.rs` (eval ↔ VM), `bundle.rs` (compile ↔ encode/decode/execute), `wasm.rs` (native ↔ wasm32 under wasmtime), `jit_differential.rs` (tier 0 ↔ forced tier 1, both codegen arms), `determinism.rs`, `leaks.rs`, `ir_corpus.rs`. **There is no `aot.rs`.**

What gates `--native` today is one hand-written program: `crates/noeta-cli/tests/cli/build.rs:141-186` `build_native_matches_a_source_run_byte_for_byte` — an all-int `sq`/`fib`/loop using `echo` only, comparing **stdout only**, which `return`s silently when `cargo` or `cc` is unavailable (`:150`, `:155`).

That is why the AOT tail's dropped stderr and truncated exit code went unnoticed, and why `crates/noeta-vm/src/backend.rs:589-610` `run_module_aot` can be a fifth hand-rolled VM setup ("stays off the `RunOptions` core") omitting `cancel_flag`, `hot_mailbox` and `profile_seam` without anything noticing. All are N/A *today*; a new mandatory init step added to `run_module_with` (`:216-287`) reaches four setups and not this one.

**A declared oracle exists and is wired into nothing.** `crates/noeta-jit/src/lib.rs:327` reads `NOETA_JIT_AOT` to make the runtime JIT emit AOT-form bodies (inline caches off `:506`, cancel poll off `:520`, null call sites `:2294`), and three comments (`lib.rs:971`, `crates/noeta-vm/src/tests.rs:80`, `:137`) assert "the AOT codegen is proven corpus-wide by the `NOETA_JIT_AOT` oracle". It appears in no gate script and no CI workflow. Contrast `--cancel-poll`, which got its own permanent CI arm for exactly the reason a second codegen shape needs one.

**Past drift.** `0f9752d4c fix(jit): word-align the AOT dispatch table so the runtime deref is sound` — an AOT-only soundness bug found late. And the good precedent: `2a3961af4 perf(native): shed the compiler front-end from shipped --native binaries` produced a *structural guard* (`aot_runtime_does_not_link_the_compiler_frontend`, `build.rs:273`) rather than a note.

**Chokepoint.** `crates/noeta-conformance/src/aot.rs` shaped like `bundle.rs`: for every corpus program that compiles, build the `--native` artifact and assert the whole `RunResult` matches the native VM run; plus a `NOETA_JIT_AOT` arm in `gate.sh`/`ci.yml`. **Size: ~200 lines for the oracle — link time is the real cost, so it may have to be a gate-only rather than per-commit arm — and ~4 lines for the JIT arm.** Also change `report.native_protos > 0` (`corpus.rs:216`, `:259`) from a floor to a ratchet: a regression in `is_fast_op` that dropped 2600 prototypes to bail stubs passes today.

---

## 10. `noeta serve`'s three hot paths — confirmed, and it has already dropped a feature

**CLOSED 2026-08-01**, `audit/hot-install`. The findings below are kept as written; the closing note is at the end of the section.

**Known and unfixed. Confirmed, characterized, with a smoking gun.**

Three functions in `crates/noeta-cli/src/cmd/serve.rs` assemble the same hot-reload install by hand: `run_program_hot` (`:695-771`) for the single worker, and `serve_parallel_hot` (`:576-633`) + `run_worker_hot` (`:638-687`) for the fleet.

The shared sequence, in order: check → emit diagnostics and gate on errors → `compile_with_sites_session` → build the mailbox → build the wake → `watch::spawn_hot_watcher` → build the real host → build the executor and `set_wake` → `run_module_hot` → render the tail. Nine steps, two implementations, and the only genuine difference is `HotChannel::default()` (`:727`) versus `HotChannel::new(workers)` (`:593`) — one consumer versus N.

**The drift is already there.** `serve_parallel_hot` takes `sources: &SourceMap` and its first statement is `let _ = sources;` (`:589`). Someone threaded the source map into the parallel path and then had nothing to do with it, because the step that consumes it — `render_trace` — was never copied. `run_program_hot:766-769` emits runtime diagnostics and the full traceback; `run_worker_hot:683-685` emits `[worker] aborted`. **A panic in a `--parallel` worker prints five words and no stack**, in a project whose ledger records production stack traces as shipped on both backends.

There is a third copy of the front half, in another file. `crates/noeta-cli/src/watch.rs:636-684` `relink_entry_unit` repeats resolve-graph → `load_with_deps_appending` → `context::loaded` → `check` → gate-on-errors, matching `run_file` (`serve.rs:292-360`) and `serve_parallel_impl` (`:385-449`). One difference worth a second look: the two in `serve.rs` take the compose probe's already-resolved graph when there is one (`serve.rs:301`, `:391`, "audit-5 F2"); `relink_entry_unit` always re-resolves from scratch.

Finally, `entry_tail` is called with an `EntryCall` the extension command declared (`serve.rs:315`) on one path and with one hand-written in the CLI (`:404-412`, `SERVE_ENTRY_MODULE` + `HANDLER`) on the other, under a comment asserting they are "built the same way". They are, *now*; a change to the serve entry's signature is two edits in two crates.

**Chokepoint.** One `hot_install(program, sites, consumers, host_builder)` covering steps 3–8, one `run_tail` for step 9 (row 1's chokepoint), and one `load_entry_with_tail(file, tail)` for the front half shared with `watch.rs`. The `--parallel` `EntryCall` comes from the same `ExtCommand` declaration the single-worker path reads. **Size: ~120 lines moved; the three functions become three short callers.** Row 1 subsumes the tail half, so doing that first makes this smaller.

### Closing note

**The install split in two, not one, and the row's own sentence says why.** Steps 4–6 (mailbox, wake, watcher) happen once per *process*; steps 3 and 7–9 (session compile, host, executor, run, tail) happen once per *isolate*. `serve::HotRig` is both halves — `HotRig::arm(entry, tail, baseline, front, consumers)` and `rig.run_isolate(program, sites, sources, HotIsolate { … })` — and the parameter the row predicted is the only one there is: `consumers`. `run_program_hot` arms a rig of 1 and runs one isolate; `serve_parallel_hot` folded into `serve_parallel_impl`, arms a rig of N and runs the same `run_isolate` in each. `run_worker_hot` no longer exists.

**The front half went further than the row asked.** `context::load_entry_with_tail(file, tail, Front)` is shared by all three sites, but the loader call itself moved *down* a layer into `noeta_runner::compile::load_linked_appending`, beside the `load_linked` that `run` and the tier verbs already used. The three serve loads were the only loads in the tree that called `noeta_loader` directly with a hand-assembled argument list, i.e. the only ones outside the `FrontFacts` firewall — which is a better statement of the defect than "three copies".

**The `relink_entry_unit` re-resolve was a bug, and a worse one than a wasted resolve.** `graph::resolve_graph` refreshes `noeta.lock`, and under `[trust].require_transparency` it is by definition a live registry round trip — so a hot server re-solved its dependency graph on every keystroke-save, over the network, and a transient failure came back as `RelinkError::Unreadable`, which is "no verdict": printed, skipped, the developer's edit silently discarded. It bought no freshness at all, because a change to `noeta.toml`, `noeta.lock` or any file but the entry never reaches the re-link — the watcher exits with `HOT_RESTART_CODE` and the wrapper restarts the process, which resolves from scratch. Dependency freshness is the restart's job. The boot's `FrontFacts` are now handed to the watcher (`Front::Given`) exactly as the diff baseline already was.

**The `EntryCall` was written three times, not two.** The row found `SERVE_COMMAND`'s and the CLI's hand-written twin; the third was the ABI's own default `CommandCtx::serve_parallel`, which rebuilt the call from `port`/`host` to forward it to `run_file`. The undercount is the audit's own recorded pattern. `serve_parallel(file, entry, host, port, workers)` now takes the declared call (ABI 13); `host`/`port` stay because they are the address the *driver* binds, not the call.

**Two behaviour fixes fell out of unifying, neither of them the drift the row named.** The single worker armed its watcher *after* compiling and the fleet armed *before*, so a save made during a cold compile was lost only on the single-worker path — the fleet's order won. And every serving isolate's host now comes from one `serve_host`, which is where `with_live_output` had already been forgotten twice (row 1 found those).

**Open, found here, not fixed: the fleet's wake reaches exactly two workers.** With the wake armed, an idle `--parallel N` fleet applies a deposited swap in two workers before the next request whatever N is — 0 stale responses at N=1 and N=2, 1 at N=3, 3 at N=5. With `set_wake` removed, *every* worker serves exactly one stale request (1 of 1, 3 of 3), so the wake works and simply does not reach a worker that is not awaiting it. Per-consumer `Notify`s were built and measured and changed nothing at N = 1, 2, 3 and 5, which rules out the `notify_one` single-permit race and points at where an idle worker holding a pre-bound listener actually blocks. That is a row of its own, in the host/VM, not in the install; the measurement lives in `watch::wake_all`'s doc and `parallel_hot` pins the bound (`stale < workers`) so a regression to "the wake reaches nobody" fails.

**Gates.** `cmd::serve::tests::every_step_of_the_hot_install_is_performed_in_exactly_one_place` censuses the serve/watch pair for the single call performing each step, so a second install fails the build naming the step; `parallel_hot::a_fleet_and_a_single_worker_apply_an_idle_swap_identically` (`#[ignore]`d, `hot-e2e.sh`, count bumped 1 → 2) runs one edit against both shapes and pins the idle wake. Dropping `set_wake` fails both.

---

## 11. The language's own vocabulary is listed in seven places, and four of the lists are stale

**Live (four instances). Loud enough to notice, cheap to fix, and the tooling has no way to see it.**

**Word keywords — seven lists.** The lexer's `#[token]` attributes are the source of truth (`crates/noeta-lexer/src/lib.rs:30-205`, 46 keywords; operators at `:270-408`). Restated in: `RESERVED_WORDS` (`:1600-1648`, test-only), `crates/noeta-ide/src/highlight.rs:83-132` `is_keyword` (46), `crates/noeta-ide/src/completion.rs:70-103` `KEYWORDS` (**32**), `editors/vscode-noeta/syntaxes/noeta.tmLanguage.json:307-360` (46), `editors/tree-sitter-noeta/grammar.js` (33), `editors/tree-sitter-noeta/queries/highlights.scm:74-90` (30).

`RESERVED_WORDS` only asserts census → lexer (`every_reserved_word_round_trips`, `:1653`); there is no reverse assertion, and **no test anywhere compares the lexer to the other five**. CI's `editors` job (`.github/workflows/ci.yml:269-295`) runs each grammar against *its own* corpus — none of which knows what a keyword is.

- **`isolate` is missing from `completion.rs:70`.** Present in the lexer, in `is_keyword`, in TextMate and in `highlights.scm`; the list's doc excuses only "the reflection intrinsics", which `isolate` is not. The editor silently never offers it.
- **The 13 reflection intrinsics highlight in TextMate (`:330`) and have no rule at all in tree-sitter.** VS Code and Neovim/Helix colour the same file differently.

**Builtin type names — four lists, one already closed.** `crates/noeta-ast/src/builtin_ty.rs:112-142` is the source of truth ("The only string match over built-in type names in the tree"). `crates/noeta-ide/src/highlight.rs:57-80` now decodes through `BuiltinTy` and matches exhaustively — its doc records that it replaced "a hand list that had drifted (`unit` was missing)", landed as `e8d4caf77`. **That is the model.** Still hand-listed: `noeta.tmLanguage.json:366` and `grammar.js:687-694`, both omitting `Any` (the canonical alias of `dyn`, `builtin_ty.rs:123`), with tmLanguage additionally omitting `Enum`/`Struct`/`Class`. Commit `f206bcd10` concedes the position: "Neither list can be compile-forced the way `BuiltinTy` can, which is exactly why they drift; adding the name at the same time as the type is the only defense."

**Prelude names — four lists, one stale.** `crates/noeta-builtins/src/lib.rs:12` `PRELUDE_NAMES`, `crates/noeta-check/src/env.rs:203`, `crates/noeta-check/src/subst.rs:32`, and `noeta.tmLanguage.json:375-378` — which still matches `signal|computed|effect|assert|panic|len`. `len` became a method and `signal`/`computed`/`effect` moved behind `use std.reactive` in the prelude-redesign arc; `Ok`/`Err`/`some`/`none` were never added. Silent mis-colouring in both directions.

**Past drift.** `3540ef568 fix(tree-sitter): repair query drift; ci: gate editor test suites` — `highlights.scm` carried a statically impossible pattern that made `tree-sitter test` fail before running anything, so the grammar's own tests were dead; the fix and the *creation* of the `editors` CI job are the same commit, meaning the grammars had no CI at all before 2026-07-26. `f206bcd10` applied one language change to two grammars by hand. `1f7bb38c7 feat(lexer): a token can say it is a reserved word, and what for` added `TokenKind::reserved_word()` (`crates/noeta-lexer/src/lib.rs:661`) precisely so there would not be a second hand list — a derivation none of the five downstream lists uses.

**Chokepoint.** A Rust test that reads `TokenKind`, `BuiltinTy` and `PRELUDE_NAMES` and greps the two grammar files plus the two IDE lists, asserting coverage in both directions with an explicit allow-list for deliberate omissions. The grammars are plain JSON and JS; `noeta-diagnostics` already greps its own source for the `ALL` gate, so the technique is in-house. **Size: ~120 lines, plus the four one-line vocabulary fixes.**

---

## 12. On-disk artifacts whose two halves are hand-maintained

Two instances of one shape: an artifact written and read by independent hand-written code, with nothing tying the halves together.

**`FORMAT_VERSION` and the serialized layout.** `crates/noeta-bundle/src/lib.rs:191` (`= 17`), used at `:259` (write) and `:277` (read). All seventeen bumps are explained in a ~140-line changelog at `:48-190` — an unusually good one, enforced by nothing. The round-trip oracle cannot help by construction: it encodes and decodes with the same build. There is no golden `.noeb`, no schema hash, no cross-version fixture. Add a trailing `Vec<T>` to `reflect::TypeInfo` and forget the bump, and a `.noeb` (or a `build --exe` stapled binary) written by the previous build passes both gates in `read` — fmt_ver 17 == 17, and `RUNTIME_VERSION` is `CARGO_PKG_VERSION`, unchanged during development — then postcard-decodes against the new layout, reading the new sequence's length prefix out of the next field's bytes. Reflection desynchronises into garbage rather than erroring. **Past drift: `28e5d724b fix(bundle): bump the container format — the mask changed the Module layout`.** A `fix(` commit: the bump was forgotten in the original change and landed later. (Auditing note for whoever checks this again: `git log -S "pub const FORMAT_VERSION"` shows 2 hits, `-G` shows 28 — a value-only edit does not change the occurrence count.) Same shape, flagged for whoever owns the ABI: `ABI_VERSION: u32 = 11` at `crates/noeta-ext-abi/src/lib.rs:107`.

**`noeta.lock`'s two walks.** `crates/noeta-pm/src/lock.rs`: `Lock::read` (`:140`) is a hand-rolled `table.get("…").and_then(as_str)` walk; `render` (`:341`) is a hand-rolled `push_str(format!(…))` walk. They share no schema; adding a `[[package]]` field means touching both, in two idioms, two hundred lines apart. Four fields are already write-only — `native` (`:386`), `edition` (`:389`), `source`, `path` — and that is *deliberate and documented* (`:538`). **That is the finding, not a bug**: because some fields are legitimately written and not read, "the sets should match" is not a checkable invariant, so a field that is *accidentally* write-only looks exactly like the four that are meant to be. The tests (`:490-724`) all construct `LockedPackage` by hand and cannot see it. Usually the cost is degradation — a missed pin means a re-resolve — except for `scope_trust` / `log_trust` / `advisory_trust`, where a dropped pin turns a TOFU downgrade defense into a fresh trust-on-first-use against whatever the registry currently serves. Same family as row 4, quiet in the same way. `LOCK_VERSION` (`:45`) is checked strictly (`:147`), which is the right failure direction.

**Chokepoints.** For the bundle: a stable structural digest of `Module`'s serde shape pinned in a source constant next to `FORMAT_VERSION`, so changing the layout fails until both are updated (**~120 lines**, most of it the digest walk); the cheap 80% version is one checked-in golden `.noeb` per format version, which catches the additive case that has actually happened (**~40 lines**). For the lockfile: either derive both directions from one serde schema — the `.noeb` payload's answer, see below — or a `LockedPackage` field census classifying each field `RoundTrip` or `WriteOnly(reason)` (**~80 lines**).

---

## 13. `run_module_aot` is a sixth VM setup that does not go through the core, and it says so

**Found 2026-08-01 while surveying row 3's `..Default::default()` class. No live defect proven; the same shape as row 1 one layer down.**

`Vm::run_module_with` is the run core, and it is a real one: nine or so mandatory init steps — collector mode, the safepoint-GC arm, the debug-session arena, the hot-mailbox consumer cursor (whose comment explains that claiming it *before* the run can drain is what stops a worker losing a swap), the isolate module handle, the cancel flag, the profiler seam. Eleven `run_module_*` methods are thin `RunOptions` presets over it.

`run_module_aot` (`crates/noeta-vm/src/backend.rs`) is not. It hand-rolls `Vm::load`, sets three isolate fields and `tier1.aot`, binds the dispatch table and calls `run_and_teardown` — and its own doc states the reason: *"Stays off the `RunOptions` core: the dispatch bind is an unsafe pre-run step no other mode has."* That is a true statement about `unsafe`, and it is also the entire mechanism by which this path stopped receiving `cancel`, `hot_mailbox` and the profiler. All three are N/A for a `--native` binary **today**; the point is that nothing makes the twelfth caller reconsider when a tenth init step is added to the core.

Row 9 records the consequence from the other end: `--native` is the one execution surface with no differential oracle, so this divergence is unobserved by construction. The two rows are the same finding seen from opposite sides, and doing row 9 first is what would make this one falsifiable.

**Chokepoint.** `RunOptions` grows the dispatch table as an option — a newtype carrying the safety contract, so the `unsafe` stays at the boundary where the caller asserts it rather than justifying a parallel function — and `run_module_aot` becomes the twelfth preset. **Size: ~40 lines.** Then add a census in the shape of `lowerer_field_census.rs`: every `RunOptions` field is consumed by `run_module_with`, and every `run_module_*` method reaches it.

---

## 14. The hot-swap corpus only generates swaps that need no remap, so the fix it exists to protect was never exercised

**Found 2026-08-01 by row 6's mutation testing. Not a parallel path — a gate that is weaker than its own documentation.**

`crates/noeta-vm/tests/hotswap_corpus.rs` is the strongest swap gate in the tree: four generators over the whole conformance corpus, 1566 cases, `KNOWN_DIVERGENCES` empty. It found seven of the nine divergences the compile/swap arc fixed. It cannot see the eighth kind at all.

Row 6 established this by deleting `absorb_type_args`'s type-argument remap outright — the fix for bug 3 of the original four, "the type-argument table replaced where it had to be merged". **All 50 hand-written `hotswap.rs` cases and all 1566 corpus cases stay green.** Only four unit tests over synthetic bundles fail.

The cause is structural and applies to the whole harness, not to one generator: all four generators produce **append-only** programs — clone-and-append, clone-and-change-a-body, re-run the top level, re-run and change a body. An appended entry interns its type arguments *after* the ones already in the session table, so the incoming indices equal the merged indices and every remap in the tree is the identity function. A gate that runs 1566 cases through the identity case is measuring one case.

This generalises past type arguments. **Any session-merge table whose remap is the identity under append is untested by this corpus** — which is most of `TABLE_POLICIES`' merge-by-content rows. The gates added by the compile/swap arc classify the tables correctly and check that the classification is *stated*; the corpus was supposed to check that the classification is *acted on*, and for the append-only shape there is nothing to act on.

Row 6 added one hand-written case that pins the mechanism (`a_swap_that_renumbers_the_type_argument_table_still_resolves_every_t`: v2 mentions the same three types in reverse order, so its freshly-checked table is a permutation and every index moves; the test asserts the non-identity precondition so it cannot silently degenerate). That covers the mechanism. It does not cover the corpus.

**Chokepoint.** A fifth generator that **permutes** rather than appends. The difficulty is real and is why row 6 left it: reordering top-level statements changes observable output for most corpus programs, so the generator needs a permutation that is order-insensitive by construction — reordering *declarations* (whose order is not observable) rather than statements is the obvious candidate, and would move the interning order without moving the output. **Size: ~80 lines in the harness, plus whatever the first run finds.** Worth doing before trusting the corpus about any merge-by-content table.

---

## Already tracked elsewhere — not re-reported

**The formatter's safety proxy** is `plans/fmt-structural-safety-gate.md`, which is a better write-up of that parallel path than this file would manage. Its §A (the corpus structural property, `crates/noeta-fmt/tests/structural.rs`) landed; §B (`zero_spans` + derived `PartialEq`) has not. Worth noting only that the run-time gate does hold the line for the printer's 19 `_ =>` arms: a miss at `crates/noeta-fmt/src/print.rs:3536` (`_ => u8::MAX` in the precedence table — a new `Expr` variant defaults to "never parenthesise") is a *refusal to format*, not a corruption, because `crates/noeta-fmt/src/lib.rs:316-329` reparses its own output and returns `FmtError::Safety` on mismatch.

**The ~20 independent `Expr` walks** are the subject of `crates/noeta-loader/src/ast_walk_coverage.rs`, whose module doc is the definitive statement of the class: Rust forces every walk to consider a new *variant* (the load-bearing half, and it holds everywhere), and does not force an arm to consider a new *field* (`..`). That gate closes the field half for the qualifier and says so: "There is no single walk. ~16 files match on `Expr` independently."

One new datum for whoever picks that up: the most dangerous ungated walk is `hoist_in_expr` (`crates/noeta-ir/src/lower/state_machine.rs:1088-1277`), the async desugar's await-hoisting pass. It is exhaustive over variants with no wildcard — good — and ends in a fourteen-variant leaf group at `:1262-1275`, which is the "attractive nuisance" the coverage gate warns about by name. Every arm uses `..`, so a new sub-expression field on an existing variant is not descended into and an `.await` inside it is silently not hoisted. Cheapest interim rule, the one `pretty.rs` adopted after the fmt survey: **no `..` in an `Expr` arm in a semantic walk** — bind every field, `_`-bind the deliberate ones, so a new field is a compile error at the site that must consider it.

**The VM ↔ reference-interpreter mirror** is `plans/backend-mirror.md`, with a per-item decision and a stated policy. Row 8's salsa twin is the same shape without the policy, which is why it is a row here and this is not.

---

## Checked and found fine

Recording these so the next audit does not re-walk them.

**`.noeb` payload encode/decode — clean, and the model to copy.** `Module::encode`/`decode` (`crates/noeta-bytecode/src/lib.rs:1880`, `:1885`) are `postcard` over one derived schema. No `write_x`/`read_x` pairs; adding a field is one edit. The gate (`crates/noeta-conformance/tests/corpus.rs:174` via `src/bundle.rs:108`) compiles *every* corpus program, asserts encode/decode/re-encode byte identity **and** that the decoded module executes byte-identically — strictly stronger than a hand-constructed round-trip, because a field neither written nor read changes observable behaviour. `crates/noeta-conformance/tests/determinism.rs` re-executes in a second process to catch a `HashSet` iterated into a serialized table. The bundle header pair (`crates/noeta-bundle/src/lib.rs:254`/`:268`) is four symmetric fields with a test per asymmetry; stapling (`:359`/`:371`/`:382`) is symmetric and tested; wasm slot patching refuses zero-or-several occurrences and fails loudly.

**`ProgramFacts` — fully closed.** `crates/noeta-ir/tests/lowerer_field_census.rs` machine-checks that every field of `ProgramFacts` is folded by **both** `under` and `absorb` (`crates/noeta-ir/src/lower.rs:896`, `:912`), and that exactly one `Lowerer` field is program-derived and it is the bundle. The strongest gate in the repo, and the template every proposal above borrows.

**`TABLE_POLICIES`** (`crates/noeta-compiler/src/lib.rs:465`) covers every `ModuleCompiler` and `SessionCompiler` field, machine-checked by reading the source text. Only the `Sites` input is uncovered — row 6.

**The JIT.** `is_fast_op` (`crates/noeta-jit/src/lib.rs:1177`) is a whitelist with `_ => false`, and `emit_op` re-checks and bails: a new opcode is interpreted, never miscompiled. `plan.rs` fails closed per op and per prototype. `crates/noeta-conformance/src/jit_differential.rs` runs the whole corpus at **forced** tiering against tier 0, comparing the full `RunResult` (stderr included) plus heap residency and refcount anomalies, in two codegen arms, asserting `unsupported == 0`, wired into both `gate.sh` and `ci.yml`. The "thresholds never reached in tests" failure mode does not apply. `jit_helpers()` (`crates/noeta-vm/src/tier1.rs:738`) is one table for both JIT inits with a binding guard, and the tier-1 leaf-op happy paths were consolidated into shared helpers (`crates/noeta-vm/src/dispatch.rs:4064-4160`) — both by prior audits, both with guards.

**Diagnostics — the best-gated surface in the tree; treat it as the model.** The registry is `crates/noeta-diagnostics/src/lib.rs:551` (E0001–E0075); the prose is an **exhaustive `match`** in `explain.rs`, so a new variant does not compile without an entry ("Prose lives here rather than in the wiki so it cannot drift from the codes it describes"); the docs reference page is *generated* from `noeta explain --all --format json` (`47cc2a66e`). Three gates: `lib.rs:855-880` parses its own source to assert `ALL.len()` equals the variant count, `:886-905` asserts `explain().code == code()` with non-empty title/summary and a valid group, and `crates/noeta-cli/src/cmd/explain.rs:214-239` asserts the JSON catalog is complete. The only ungated field is `Explanation.docs`, a wiki slug not checked against `docs/*.md` — **zero misses today**, so a latent rot path rather than a defect.

**The `Host` trait's required-method core** is compiler-forced across all four impls (`SandboxHost`, `RealHost`, `WasiHost`, `BrowserHost`): `FileReader`/`FileSystem`/`Rng`/`Clock`/`Entropy`/`Ids`/`Env` have zero default methods. `Network`'s ~20 defaults all return an explicit `Err("this host does not …")` with `net_ws_is_closed` defaulting to `true` — deliberate capability-optionality, loud at runtime. There are only three `cfg(target_family = "wasm")` sites in the tree, so **there is essentially no cfg-gated twin-body drift risk**: wasm portability is achieved with one body, which is why the wasm oracle is meaningful at all. (The oracle's own variable is the VM compiled to wasm32 versus native — a *portability* gate. `noeta-wasi-host`'s 848 lines of real capability impls are exercised only by `crates/noeta-wasm-runner/tests/runner.rs` and one `wasi:http` e2e script. No drift found; noted because "the wasm gate is strong" is true about portability and not about the host.)

**`BuiltinTrait`** (`crates/noeta-types/src/traits.rs`) is a fieldless enum with all metadata behind one `info` match, and the operator → method correspondence is kept in lockstep with `BinaryOp::overload_method` **by a unit test in the same file**. A comment that names its twin *and* points at the test enforcing it — the shape every "MUST match" comment in this report should aspire to.

**The tooling surfaces are mostly one implementation already.** `noeta-lsp` is a pure wire adapter over `noeta_ide::DocumentStore` (its `Cargo.toml:9` says so), and MCP's language tools and `crates/noeta-playground/src/ide.rs` go through the same store. `noeta-dap` runs on `noeta-runner` — "the shared compile front half (deps + tiers + editions resolve exactly as `noeta run`)". `FmtConfig` has one parser (`crates/noeta-fmt/src/config.rs:1`) shared by the CLI and the LSP. `SYNTAX.md` is generated from `docs/*.md` (`crates/noeta-cli/src/cmd/init.rs:202`), not an eighth keyword list. The gap is row 5, not the plumbing.

**`noeta-db` reuses the loader's linker** rather than reimplementing it: `workspace_with_deps` builds inputs and calls `noeta_loader::link_parsed_with_deps` (`crates/noeta-db/src/lib.rs:1003`). Its one deliberate asymmetry — passing `None` for the resolved native-package set so the editor stays lenient on foreign roots — is stated at `:1001` and errs in the safe direction (the editor never over-flags). `DepSources` mirrors `DepPackage` with a documented reason for each field it does not carry. (Row 8 is a different part of the same crate.)

**`shape.rs::render`** (`crates/noeta-ast/src/shape.rs:53`) is one parameterized walk behind `type_spelling` and `type_source`, and its doc records that "three copies of this match had drifted apart into two contracts before anyone noticed there were three". Already fixed; noted because it is row 7's shape and shows the fix works.

**`noeta-ast`'s own `Expr` walks** (`span`, `mentions`, `has_await`, `crates/noeta-ast/src/lib.rs:2270-2460`) are exhaustive with no wildcards, each documenting which direction of approximation is safe. `mentions` over-approximates by design; `has_await` is "total over `Expr` so it can never miss an await".

**The fmt corpus gates.** `crates/noeta-fmt/tests/structural.rs:132` sweeps 1251 of the repo's 1256 `.noe` files in two configurations comparing derived-`Debug` dumps — exhaustive by construction — plus `tests/corpus.rs:135` for safety and idempotence, both in CI via `cargo test --workspace`.

**Profiler and coverage output** is `serde_json` with no in-tree reader — no round-trip path, no risk.

---

## Suggested order

1. `pm.rs:1239` (row 4a) and the `noeta-pm` CI features (row 4b) — ~15 lines, and the only place where the current behaviour is *silence about a security check*.
2. `native.stderr` in the wasm oracle (row 1) — one line, and it makes the rest of row 1 fail loudly instead of needing to be argued.
3. The three live one-liners: the cache-key gaps (row 2), `package_uses` at `impact.rs:517` and `execute.rs:399` (row 3), the four stale vocabulary entries (row 11).
4. Rows 1, 10 as one pass, then row 3's constructor and row 5 as the next — they are four views of "the tail, the install and the options are copied", and the chokepoints overlap.
5. Rows 6, 7, 8, 9, 12 as separate small arcs.

---

## Status after the first pass (2026-07-31)

Fixed and merged: the `noeta audit` silence (row 4's first half) and the `noeta-pm`
CI gap (`--all-features`, not a feature list — 57 tests, 23% of the crate, had
never run); the wasm oracle's missing `stderr` (row 1's tripwire); the
startup-cache key (row 2 — `open_startup_cache` and `key_deps` now *destructure*,
so a new field is a compile error rather than an omission); the spurious E0036
(row 3 — four sites, two the audit had not found, plus a `CheckOptions` census);
`isolate` missing from completion (row 11, with a test deriving the list from the
lexer's own tokens).

**Open, in the order they now matter:**

1. **The wasm differential is RED and has been skipping.** `reflection/field_specs_of_native_struct.noe`
   diverges on *stdout* — `construct` does not build a `Frame` under wasm — and
   that comparison predates this pass, so the gate has been failing whenever it
   ran at all. It mostly does not run: `wasmtime` is not on `PATH` (it is at
   `~/.wasmtime/bin/wasmtime`) and `wasm32-wasip1` is installed for `stable`, not
   the `1.97.0` gate pin, so the step SKIPs. A gate that skips is how a red gate
   stays red — fix the environment detection first, then the divergence.
2. **Row 1's chokepoint.** With the oracle fixed, three corpus programs now fail
   on `stderr` (`io/streams`, `io/to_string_parity`,
   `http_server/serve_handler_abort_is_reported`). They are the seven hand-written
   run tails, failing loudly at last. One tail, seven callers.
3. **`CheckOptions::for_workspace(...)` + `#[non_exhaustive]`** — endorsed by the
   agent that fixed all four of its bugs: `..Default::default()` converts "I did
   not consider this field" into "I chose the default", and the compiler cannot
   tell them apart. The census merged here is a stopgap and says so.
4. **A grammar census** (~120 lines, Rust, reading the JSON/JS as text) for the
   TextMate and tree-sitter keyword/intrinsic lists. Four drifts are known and
   were deliberately NOT hand-edited: hand-editing the files whose problem is
   silent hand-editing is the wrong trade.
5. Rows 5–12 as originally written, plus: `MANIFEST.sha256` is still
   self-referential (the new CI step catches a local edit, not an un-propagated
   copy), and `list_advisories` classifies a malformed feed body as `Network`, so
   a JSON-shape drift still degrades to a note.

## A drift class the wasm work surfaced: fixtures that live in strings

`79352778` tightened `?` into E0012 and swept the tree — six conformance fixtures, the docs,
`noeta-check`'s own tests. It missed two, and neither was an oversight: the `wasi:http` e2e's
handler is a **heredoc inside a shell script**, and its native twin is a **Rust string literal**
whose `compile()` helper never runs the checker. Both are invisible to a sweep that greps for
`.noe` files, and both sat behind a gate that was skipping.

The same class ate an http-client arc one cycle earlier (`ee2ac7c6`: "the corpus, docs, and this
crate's lib test — but not the e2e script's heredoc fixture"). **Twice is a class.**

The tree is clean today — the only remaining embedded `?` is top-level code, where deferring is
correct. What is missing is anything that would notice next time. Options, cheapest first: have the
language-rule sweeps grep string literals and heredocs too (a checklist, so discipline); make the
Rust-side twins run the checker rather than only the compiler (removes half the class outright);
or extract embedded fixtures into real `.noe` files the corpus already walks (removes it entirely,
at the cost of the e2e scripts being self-contained).
