# Audit: Compiler front-end & middle-end (lexer → parser → check → IR → compiler/eval, loader, diagnostics, editions)

## Finding 1 — The formatter silently rewrites surface syntax because the parser desugars sugar it is documented not to desugar

**Severity: high** (verified end-to-end, user-visible defect + violated documented principle)

**Evidence**
- `ARCHITECTURE.md:118`: *"Surface sugar stays in the AST. Constructs like `?T`, `|>`, `~`, `?`, `??` are distinct AST nodes (not desugared in the parser) so later passes can produce precise diagnostics."*
- But the parser desugars at least four constructs with no AST marker:
  - set literal `#{a, b}` → `[a, b].to_set()` (`crates/noeta-parser/src/lib.rs:1555`, recovery helper `set_sugar_items` at `:321`)
  - compound assignment `x += 1` / `x ~= y` / `x ??= y` → `x = x OP y` (`AssignKind`, `crates/noeta-parser/src/lib.rs:619-627`)
  - spread list `[...a, x]` → `[] ~ a ~ [x]` fold (`desugar_list_literal`, `:666`)
  - `if c then a else b` → a two-arm `Expr::Match` (`desugar_if_then_else`, `:633`)
- The formatter prints from the AST, so it needs a bespoke *resugaring heuristic* per desugar. Only two exist: `if_then_else_form` (`crates/noeta-fmt/src/print.rs:1806-1858`, which sniffs whether the raw source at `span.start` begins with the token `if`) and the spread case. The doc there states the intent explicitly: *"so `noeta fmt` round-trips the surface form the author wrote."*
- **Executed proof** (probe binary linked against the audit checkout):
  - `s = #{3, 1, 2}` → formats to `s = [3, 1, 2].to_set()`
  - `n += 2` → `n = n + 2`
  - `s ~= "b"` → `s = s ~ "b"`
  - `n ??= some(2)` → `n = n ?? some(2)`
- The fmt safety gate structurally cannot catch this: it asserts *output re-parses to the same AST* (`crates/noeta-fmt/src/print.rs:9-10`) — and the desugared spelling re-parses to the identical AST by construction. The corpus harness (`crates/noeta-fmt/tests/corpus.rs`) asserts only safety + idempotency, never input-preservation, so `tests/conformance/collections/set_literal.noe` et al. pass while being rewritten on a real `noeta fmt` run.

**Why it matters** — Running `noeta fmt` on any codebase using `#{…}` or compound assignment destroys the author's surface syntax, permanently (the rewrite is idempotent, so it looks "formatted"). Architecturally it shows the desugar-in-parser pattern accreting: each new desugar eventually demands a source-text-sniffing inverse in the formatter, and two of four never got one.

**Proposed remedy** (incremental)
1. Immediate: add the two missing resugaring recoveries in `print.rs` (set literal via `set_sugar_items`, compound assign by comparing the binding's source text at `span.start` — same technique `if_then_else_form` already uses), plus a corpus assertion that files containing `#{`/`+=`/`~=`/`??=` format to themselves.
2. Right fix, per the codebase's own principle: make these real AST nodes (`Expr::SetLit`, `Stmt::CompoundAssign { op }`, `Expr::IfThenElse`) and move the desugar to `noeta-ir::lower` (the layer that already owns semantic desugaring). Checker/lowering churn is mechanical; fmt heuristics then get deleted.

**Perf-regression risk:** none.

---

## Finding 2 — `noeta-check/src/lib.rs` is a god-file around a 45-field god-context

**Severity: high** (by mandate; the file is 8,115 lines and still absorbing every new language feature)

**Evidence**
- `crates/noeta-check/src/lib.rs` — 8,115 lines; `struct Checker` (`:917-1125`) has **45 fields** mixing at least six concerns: symbol tables (`enums`/`functions`/`records`/`methods`/`type_kinds`/…), import binding (`modules`/`namespaces`/`imported_fns`/`extern_types`), effect/coloring state (`current_ret`/`current_yield`/`current_async`/`concurrent_depth`/`loop_depth`), privacy state (`current_type`/`in_dev_tier`), codegen-hint output (`sites: SiteMaps` — 14 span-keyed maps), destructor-relevance analysis (`destructor_classes`/`destruct_reachable`/`relevance`), and config (`registry`/`editions`/`record_expr_types`/`session_mode`).
- Giant functions: `collect` (`:1719-2147`, ~430 lines), `check_stmt` (`:2327-2713`), `synth_inner` (`:4308-5140`, ~830 lines), `synth_call_inner` (`:5576-5907`), trait/derive machinery (`:3475-4027`), plus ~860 lines of free helper functions (`:7250-8115`).
- Extraction has started (`tiers.rs` 1,292, `stdlib.rs` 925, `packed.rs` 333, `attributes.rs` 240) — proving the file *is* separable — but the last four arcs (kernel methods, namespaces, editions, sessions) all landed in `lib.rs`.

**Why it matters** — Every language feature makes one file the merge-conflict and comprehension bottleneck; the bidirectional core (`check`/`synth`) is ~15% of the file yet is buried among concerns that don't need its internals.

**Proposed remedy** — see the decomposition sketch at the end of this report. Key property to preserve: everything stays `impl Checker` (split across files, Rust allows it), so no signature changes and byte-identical diagnostics; the differential corpus and `check/src/tests.rs` gate each move.

**Perf-regression risk:** none (file moves, no dispatch changes).

---

## Finding 3 — `use`-import classification is encoded three-plus times; name binding is the smeared half of linking

**Severity: medium** (partially documented as debt; the concrete instance is worse than the docs imply)

**Evidence**
- The declared single source of truth: `Registry::classify_use` (`crates/noeta-native/src/registry.rs:1112`), used by the checker (`crates/noeta-check/src/lib.rs:1658`) and the IDE (`crates/noeta-ide/src/completion.rs:151`), with an exhaustiveness test (`registry.rs:1817`).
- The bytecode compiler reimplements it: `is_native_module` / `selective_import_module` / `qualified_module` (`crates/noeta-compiler/src/lib.rs:97-130`), consulted at three lowering sites (`:849`, `:1022`, `:1958`).
- The eval backend reimplements it a third time, inline (`crates/noeta-eval/src/lib.rs:1615-1630`: `rooted` / `selective_module` / `find_module` chain).
- The loader deliberately *retains* unresolved std/native `use`s "for the compiler's downstream binding" (`crates/noeta-loader/src/lib.rs:566-571`), i.e. link-time name binding is by design finished per-backend — so the classification fork is on the trust path. `ARCHITECTURE.md:115` acknowledges mirrored routing as known debt, but frames it as backend *dispatch*, not import *classification*.

**Why it matters** — A new import shape (the registries arc keeps adding them: nested modules, selective members, namespace groups, extern types, third-party roots) must be taught to three matchers. A miss diverges checker-accepted programs from backend binding; the differential catches it only if a corpus case exercises the exact spelling on both backends.

**Proposed remedy** — Make both backends consume `classify_use`'s `UseKind` instead of re-deriving it: the compiler's three call sites and eval's one already have `path`/`name`/registry in hand, so this is a mechanical substitution; delete the private helpers. (Both crates already depend on `noeta-stdlib` → `noeta-native`.)

**Perf-regression risk:** none (same lookups, one function).

---

## Finding 4 — Editions: the loader lexes every source under `Edition::DEFAULT` while parsing per-package

**Severity: medium** (fresh seam, incompletely threaded on exactly the leg the docs predict will matter)

**Evidence**
- `crates/noeta-loader/src/lib.rs:466-490` — `lex_program` lexes each file with plain `lex()` and re-lexes (text-tier pass) with `noeta_lexer::lex_in(source, noeta_lexer::Edition::DEFAULT, &set)` (`:487`) — even though the per-source `editions` map is fully built *before* the call (`link_with_deps`, `:356-390`) and the subsequent parse is correctly per-package (`parse_clean`, `:498-504`: *"Parse under the owning package's edition"*).
- The edition crate's own docs name tokenization as the first thing an edition will gate: `crates/noeta-lexer/src/lib.rs:662-663` (*"a future edition that promotes an identifier to a keyword, or changes a literal's syntax"*), and `noeta-edition/src/lib.rs:19-22` claims the value is *"already at the point that would consult it."*

**Why it matters** — Today it's byte-identical (one edition; `lex_in` ignores the argument). But the entire point of the arc — stated in its own doc comments — was to have the plumbing done *before* the first divergent edition. The multi-package lex path is the one place where that claim is false: the day an edition changes lexing, dependency packages lex under the wrong edition and the failure will be a confusing parse error, not a threading error.

**Proposed remedy** — Pass the already-constructed `EditionMap` (or a per-source edition slice) into `lex_program` and thread each source's own edition into both lex passes. ~10 lines; add a loader unit test asserting `lex_in` sees the dependency's edition (mirror of the existing `the_edition_map_keys_every_source_by_its_package` test at `:1131`).

**Perf-regression risk:** none.

---

## Finding 5 — `noeta-eval` is fully live and dep-graph-enforced test-only, but its self-description is systematically stale

**Severity: medium** (in a codebase that explicitly calibrates on intent-comments, wrong intent-comments are a defect)

**Evidence — what the crate actually is** (the mandate's question):
- **Live, not vestigial.** `lib.rs` (5,929) is the shared execution engine — `Value` model, `Scope`, `Interpreter` (builtins, method dispatch, native marshalling, tasks/channels, ~3,000 lines) — and `ir.rs` (2,072) is the Core-IR orchestration that drives it (`crates/noeta-eval/src/ir.rs:6-14`). There is no AST expression evaluator left (no `eval` over `Expr`; the only AST manipulation is the REPL trailing-expr rewrite at `:285`). `cargo check -p noeta-eval` is warning-clean — no dead code.
- **Test-only is enforced by the dependency graph, not convention:** the only production-graph reverse-dependency is `noeta-conformance` (dev-only harness); `noeta-cli` does not depend on it (`crates/noeta-cli/Cargo.toml:24-53`), and `noeta run` executes via `noeta-runner` → `VmBackend` (`crates/noeta-runner/src/lib.rs:19,60`). The other `noeta-eval` mentions in Cargo.tomls are comments only.

**Evidence — the stale narration:**
- `crates/noeta-eval/src/lib.rs:1-9`: *"The evaluator: an AST → a RunResult … the M0 tree-walker … M0 scope grows one vertical slice at a time."* False on all counts.
- `lib.rs:49-54` claims *"The plain `Backend::run` path remains … (an AST-walk baseline, not an oracle)"* — directly contradicted by the implementation 14 lines below, which lowers to IR (`:68-101`).
- `ir.rs:95-107` and `:120-125` claim `run_ir`/`run_ir_with_host` are *"the same path `lang run` … take[s]"* / *"`lang run` uses this"*. False — only `noeta-conformance/src/reference.rs:73,107` calls them. (Note also the dead product name "lang".)
- `ir.rs:16-23`: *"destructors fire **only** at global teardown (never on a local or temporary drop)"* — describes the pre-migration world; the crate itself runs `insert_drops` with destructor relevance (`lib.rs:95-99`) precisely to fire last-use destructors.
- Same disease in `noeta-compiler/src/lib.rs:15-26` (*"every M0 corpus program … asserted identical to the tree-walker"*, `Unsupported` list naming long-fixed gaps) and `:46-48` (*"Registers are allocated monotonically (one per value, no reuse)"* — true only pre-`regalloc.rs`, which the doc doesn't mention; `regalloc.rs:1-14` states the correction).

**Why it matters** — `AGENTS.md`/`ARCHITECTURE.md` position doc comments as the calibration source for agentic development, and the audit brief itself says "check whether a nearby comment explains the design." Here the nearby comments describe an architecture that was deliberately removed; an agent trusting `ir.rs:120` would conclude eval is a production path and, e.g., preserve its perf or extend its Host plumbing.

**Proposed remedy** — A documentation-only sweep of the three module headers (eval `lib.rs`, eval `ir.rs`, compiler `lib.rs`) to the post-migration story: rename or alias `TreeWalkBackend` → `IrRefBackend` (keep a deprecated re-export), and state the true role: "reference Core-IR interpreter; consumed only by noeta-conformance." Cheap, high leverage.

**Perf-regression risk:** none.

---

## Finding 6 — Site-map plumbing is triple-bookkept: `SiteMaps` → `Sites` → `LoweringSites`, with seven hand-written field-for-field projections

**Severity: medium**

**Evidence**
- The same 11–14 span-keyed maps are declared three times with near-identical doc comments: `SiteMaps` (`crates/noeta-check/src/lib.rs:816-875`), `Sites` (`:132-191`), `LoweringSites` (`crates/noeta-ir/src/lower.rs:74-111`).
- The `Sites` → `LoweringSites` projection is hand-copied at **seven** call sites: `noeta-compiler/src/lib.rs:334,529`, `noeta-eval/src/lib.rs:80`, `noeta-conformance/src/reference.rs:51,87`, `noeta-conformance/tests/peak_memory.rs:90`, `…/index_field_fusion.rs:31` — plus `lower()` itself building 11 empty locals to fill the struct (`lower.rs:117-146`).
- The bundle concept itself is well-defended (`Sites` doc `:121-130` explains the trade-off, including that "the differential oracle is what catches a forgotten semantically-relevant map"). What is *not* documented as intentional is the sevenfold copy of the identical projection.

**Why it matters** — Adding one site map today touches ~10 locations across four crates. Worse, the seven copies can drift *individually*: a driver that forgets one field compiles fine (the struct literal is total per-site, but nothing forces the field to come from `sites` rather than an empty map) and for perf-only maps the differential is blind — exactly the failure mode for maps like `f32_literal_sites` whose omission *is* semantic, versus `index_field_sites` whose omission is silent perf loss.

**Proposed remedy** — Add `impl<'a> From<&'a Sites> for LoweringSites<'a>` (all fields are borrows; lifetimes line up) plus a `LoweringSites::empty()` backed by `OnceLock` statics; replace the seven literals. Optionally collapse `SiteMaps`/`Sites` into one type with the private `expr_types` alongside.

**Perf-regression risk:** none.

---

## Finding 7 — Statement termination is decided by two coexisting, differently-parameterized algorithms

**Severity: medium** (documented, but I am challenging the accretion)

**Evidence**
- Algorithm 1: lexer `insert_terminators` (`crates/noeta-lexer/src/lib.rs:1036-1072`) synthesizes `;` tokens at newlines, gated on `is_statement_ending(prev)`, with **absolute** `(`/`[` depth and no `{}` tracking.
- Algorithm 2: `newline_terminator_offsets` (`:1081-1127`) computes a *second* offset set with the statement-ending gate **removed** and depth measured **relative to the innermost `{`** (`{` saves/resets depth, `}` restores), consumed by the parser as a soft terminator (`crates/noeta-parser/src/lib.rs:2086-2125`). Each was added to fix a case the other missed (generic-close `>`, multi-statement closure bodies inside call parens — the doc at `lexer:1085-1098` narrates both warts honestly).

**Why it matters** — "When does a newline end a statement" is now the union of two rules with different depth semantics, one materialized as synthetic tokens and one as a peeked side-table, interacting through `is_leading_continuation` (shared) and the parser's terminator ordering. Every future token-kind addition must be evaluated against both. This is the definition of accretion: each patch is locally principled; the composite is only understandable by reading four functions and their tests.

**Proposed remedy** — Converge on one mechanism: the offsets-based soft terminator already subsumes synthetic `;` (it's the same predicate minus the ending gate; the parser could apply the ending gate itself since it knows whether a statement is complete). Keep `insert_terminators` output for one release behind a differential over the lexer snapshot corpus, then delete it. This is a contained refactor with a strong existing test net (`parser:3503-3530` covers the tricky cases).

**Perf-regression risk:** low (one fewer token pass; the offset scan already runs).

---

## Finding 8 — The pipeline "drift firewall" covers one path; a dozen CLI verbs still hand-assemble load→activate→check, keeping edition threading discipline-based

**Severity: medium**

**Evidence**
- `crates/noeta-runner/src/compile.rs:1-5` declares itself *"ONE implementation — the drift firewall"* for run/dump/build, and correctly does `load_with_deps → activate_tiers_with → check_all_with_editions` (`:169-203`).
- But `noeta_check::check_all_with_editions(&…, editions.clone())` is *also* hand-assembled at ~12 other CLI sites (`crates/noeta-cli/src/lib.rs:2249, 2544, 2551, 2883, 3277, 3517, 4508, 4868, 5072, 5387, 5760`, `watch.rs:421`), each independently responsible for remembering to thread `linked.editions`.
- Nothing catches a forgotten `editions` argument: with one edition, `check_all` and `check_all_with_editions` are byte-identical (that's stated in `check/lib.rs:255-259`), so a verb that dropped the map would regress silently until the first divergent edition ships.

**Why it matters** — This is the classic tramp-data hazard: `Linked` already owns both `program` and `editions` (`crates/noeta-loader/src/lib.rs:34-52`), yet the pair is split apart at every call site only to be re-joined inside the checker.

**Proposed remedy** — Add `noeta_check::check_linked(&Linked) -> Checked` (or `check_all_with(CheckOptions { editions: linked.editions.clone(), .. })` via a `From<&Linked>` for `CheckOptions`) and migrate the CLI sites; make the naked `check_all_with_editions` pattern lint-greppable. Longer term, fold tier activation + check into `noeta-runner`'s front half for the non-run verbs too.

**Perf-regression risk:** none.

---

## Finding 9 — The options-struct lesson from `CheckOptions` was not propagated to the compiler and lowering entry families

**Severity: low-medium**

**Evidence**
- The checker consolidated: `CheckOptions` (`crates/noeta-check/src/lib.rs:193-209`) exists explicitly *"instead of the checker growing a `_with_types_and_editions_and_registry` combinatorial family"* — good.
- The compiler grew exactly that family anyway: `compile` / `compile_with_sites(program, sites, real_isolates: bool, debug: bool)` / `compile_with_sites_session` / `compile_with_sites_session_with_registry` (`crates/noeta-compiler/src/lib.rs:135, 158, 234, 255`), with two positional bools at every call site (e.g. `noeta-cli/src/lib.rs:3467`: `compile_with_sites_session(&program, sites, false, false)` — unreadable at the call site).
- Lowering likewise: `lower` / `lower_with_sites` / `lower_with_sites_opts(program, sites, real_isolates, registry)` (`crates/noeta-ir/src/lower.rs:117, 152, 173`).
- Inconsistently, the session checker takes only editions (`check_all_session_with`, `check/lib.rs:326`) — no registry or `record_expr_types` — so the session path can't express what the batch path can without a separate `SessionChecker::with_registry` (`:410`).

**Proposed remedy** — `CompileOptions { real_isolates, debug, registry }` and `LowerOptions { real_isolates, registry }` mirroring `CheckOptions`, with the existing functions kept as thin presets (the codebase's own documented pattern). Unify the session-checker constructors on `CheckOptions`.

**Perf-regression risk:** none.

---

## Finding 10 — REPL trailing-expression desugar and sentinel are copy-pasted between the two session backends

**Severity: low**

**Evidence** — `rewrite_trailing_expr` + the `"\0repl-value"` sentinel exist verbatim twice: `crates/noeta-eval/src/lib.rs:279-306` (`REPL_VALUE`) and `crates/noeta-vm/src/session.rs:758-786` (`SENTINEL`) — same doc comment, same body, two constants that must stay equal in behavior. The `session_parity` differential (`crates/noeta-conformance/tests/session_parity.rs`) is the only thing holding them together.

**Why it matters** — This is surface-level *language* semantics (what a REPL entry's value is) duplicated across backends, unlike the intentional value-model duplication. A future tweak (e.g. capturing a trailing `if`-expression differently) edits one and relies on a test to remember the other.

**Proposed remedy** — Move it to `noeta-ast::desugar` (which already exists for expression-tier desugars) with the sentinel constant beside it; both sessions import it. 20-minute change.

**Perf-regression risk:** none.

---

## Finding 11 — The checker's documented "single error funnel" is not real

**Severity: low**

**Evidence** — `Checker::error` claims *"The single place the checker constructs an error — every diagnostic site funnels through here"* (`crates/noeta-check/src/lib.rs:1177-1180`), but ~10 sites push `Diagnostic::error(…)` directly onto `self.diags`, bypassing it (`:1208, :1249, :4412, :4426, :5658, :5749, :5784, :6986`, …). Meanwhile the sub-modules (`tiers.rs`, `attributes.rs`, `packed.rs`) emit through return values, a third pattern.

**Why it matters** — Mostly hygiene today, but the funnel is where per-diagnostic policy would land (the editions arc's edition-gated lints are the stated next step, and `edition_at` at `:1163-1175` is explicitly waiting for exactly such a rule). If S3 adds edition or severity logic to `error()`, the bypasses silently miss it.

**Proposed remedy** — Convert the direct pushes to `self.error(…)` / `.with_help` chains (mechanical; the helper already returns `&mut Diagnostic` for the label case, and a `with_label`-style `&mut` twin covers the rest).

**Perf-regression risk:** none.

---

## Finding 12 — Frontend has no symbol interning; the checker's environment clones `Type` trees on every name lookup

**Severity: low** (flagging as an LSP-latency lever, not a correctness issue)

**Evidence** — Everything in the front/middle-end is string-keyed: the 45-field `Checker` holds ~20 `HashMap<String, …>`/`HashSet<String>` tables; `Env = Vec<HashMap<String, VarBinding>>` and `lookup` clones the (Box-recursive) `Type` per identifier reference (`crates/noeta-check/src/lib.rs:741-747`); IR `Atom::Var { name: String }` (`crates/noeta-ir/src/lib.rs:90`) makes the reference interpreter resolve variables by string. Name interning exists only at the bytecode boundary (`NameId`, `crates/noeta-compiler/src/lib.rs:772-810`). No frontend interner exists anywhere (verified by search).

**Why it matters** — The checker runs per keystroke under the salsa graph (LSP), and per-lookup `Type::clone` cost scales with annotation complexity (generics arcs keep deepening types). The eval-side string scopes are fine (oracle only). This is not accidental O(n²), but it is a standing allocation tax on the hottest IDE path.

**Proposed remedy** — Don't boil the ocean: (a) return `&Type` from `lookup` (callers that need ownership clone at the few mutation points), (b) consider `Rc<Type>` inside `VarBinding`. A full `Symbol` interner is only worth it if LSP profiles say so.

**Perf-regression risk:** none to low (strictly removes clones; needs care around `assign`'s in-place type update).

---

## Finding 13 — `DiagnosticCode` requires three parallel hand-maintained lists

**Severity: low**

**Evidence** — Adding a code means appending to the enum (`crates/noeta-diagnostics/src/lib.rs:17-224`), to `ALL` (`:229-282`, whose comment concedes *"Append new variants here as well as in code()"*), and to `code()`'s match (`:286-341`). `#[non_exhaustive]` + the match keeps enum/`code()` in sync via compile error, but `ALL` is only conventions — a forgotten `ALL` entry silently breaks `from_code`, which the conformance runner uses to validate `// expect:` headers (`:343-345`), i.e. a mistyped expectation would stop being caught.

**Proposed remedy** — One declarative macro (`codes! { UnexpectedCharacter => "E0001", … }`) generating enum + `ALL` + `code()` + doc comments, or a unit test asserting `ALL.len()` equals the variant count via a generated exhaustive match.

**Perf-regression risk:** none.

---

# Answers to the mandate's cross-cutting questions

**How many times is language semantics encoded?** For an executable construct, typically 5–7 places: parser grammar (+ its desugars, Finding 1), checker typing rules, `noeta-ir::lower` (the sanctioned semantic desugar point), then *two* IR consumers (`noeta-compiler` IR→bytecode and `noeta-eval/ir.rs`), then the VM's op implementations (`noeta-value/src/ops.rs`) and the JIT's native fast paths. Plus two inverse encodings (fmt printer, tree-sitter grammar) and one mirror table (`check/src/stdlib.rs`, safely-degrading by design per its header). Concrete 3+-place cases with drift history: **value equality** (`noeta-eval/src/ops.rs:240` vs `noeta-value/src/ops.rs:438` — the latter's comment says *"Mirrors the tree-walker's `values_equal`"* — plus JIT paths; the maps-equality silent-wrong bug happened here and left the guard `tests/conformance/collections/equality_over_all_kinds.noe`), **fixed-width arithmetic** (`apply_binary_wide`, eval `ops.rs:59`, self-described as *"the tree-walker twin of the VM's… Same helper and error text"*, plus JIT native WideInt), and **use-import classification** (Finding 3, the one *not* oracle-covered per spelling). What holds them together: the 617-file conformance corpus run `--differential` with 0 skips, `--jit-differential` (byte-identity + zero leaks), `session_parity`, leaks/peak-memory checks, per-stage insta snapshots, and targeted guard cases. This is a deliberate, well-defended architecture (`ARCHITECTURE.md:113-115`); the residual risk is confined to behavior *invisible to `RunResult`* (perf-only site maps, Finding 6) and *untested spellings* (Finding 3).

**noeta-eval live vs. vestigial** — fully live (Finding 5): ~200 lines of REPL `Session` (oracle for session parity), the 3k-line `Interpreter` engine driven by `ir.rs`, and the value model. Zero AST-walk code remains; what's vestigial is the *narration* and the `TreeWalkBackend` name. Test-only status is enforced by the dependency graph (only `noeta-conformance` depends on it), not just convention.

**Loader/linker** — linking itself is a clean, single-phase core (`link_core`, `merge_module_closure`'s worklist fixpoint at `crates/noeta-loader/src/lib.rs:843-891` is tight and well-documented; dedup by qualified identity, cycle-safe). What's smeared is the *binding of retained imports* downstream (Finding 3). The loader's coupling to the process-global registry (`:581`) is documented and consistent with the instance-registry arc's stated single-registry stance for tools.

---

# What's already good

- **The differential-oracle spine.** Two genuinely independent execution stacks meeting only at `Backend`/`RunResult`, gated by a 617-file corpus with a zero-skip policy, JIT and session variants, leak/refcount assertions — this is the strongest duplication-control regime I've seen at this scale, and it demonstrably works (the equality bug produced a permanent guard case).
- **Diagnostics discipline.** One catalog, stable append-only codes, exactly one `ariadne` dependency in the workspace (verified), typed `Diagnostic` values everywhere, `closest()` shared for did-you-mean. The claim in `ARCHITECTURE.md:116` holds.
- **`CheckOptions` and the `Checked`/`Sites` split.** The single-configurable-entry pattern with documented thin presets, and the "run the checker once, thread the bundle" fix for the 2–3× re-check, are exactly right — Finding 9 is just about finishing the rollout.
- **The edition seam's design** (as distinct from Finding 4's gap): a dependency-free bottom crate, closed validated enum, `EditionMap` keyed by `SourceId` reusing the span→source machinery, cache-key inclusion, honest `#[allow(dead_code)]` on `edition_at` with the rationale written down.
- **Parser robustness engineering:** `recovering_list` skip-to-terminator recovery, the `nesting_depth` pre-pass + big-stack worker turning stack overflow into diagnostic E0032 (`parser:770-845`), and `rich_to_diag` mapping chumsky errors into the catalog.
- **Deliberate, explained trade-offs everywhere.** `Sites` bundling, the loader's lenient-vs-complete `RetainPolicy`, `check/stdlib.rs`'s fall-to-`dyn` mirror table, regalloc's three safety invariants — the intent-comment culture is real; Finding 5 is severe *because* the culture is otherwise trustworthy.
- **`noeta-runner::compile` as an extracted drift firewall** for the run/dump/build path — the right instinct; Finding 8 asks it to finish the job.

---

# Decomposition sketch for `noeta-check/src/lib.rs`

Keep one `struct Checker` (possibly with fields grouped into sub-structs) and split the `impl` across modules — no public API change, snapshot- and corpus-gated per move. Suggested cut lines, in extraction order (safest first):

```
noeta-check/src/
  lib.rs            — entry points (check_all* presets, CheckOptions, Checked/Sites),
                      check_all_impl orchestration, SessionChecker. ~600 lines.
  env.rs            — Env, VarBinding, lookup/bind/assign free functions (`:734-807`),
                      FnSig/GenericInfo/VariantInfo. Pure, zero Checker coupling. [trivial]
  sites.rs          — SiteMaps + Sites + into_sites + DestructorRelevance (`:816-900,
                      1135-1150`) and the record_* helpers. [trivial]
  prelude.rs        — register_prelude / seed_extern_type_traits / register_extension_attributes /
                      register_semantic_prelude / register_tier_prelude / register_type_enum
                      (`:1315-1525`). Write-only seeding of Checker tables. [easy]
  relevance.rs      — compute_relevance / record_param_relevance* / type_relevant fixpoint
                      (`:1525-1645, 7965-8001`). A pure analysis with its own output type;
                      arguably its own crate later (it serves ir-passes, not typing). [easy]
  collect.rs        — pass 1: collect_imports + collect + record_optional_fields/record_derived/
                      record_attribute (`:1645-2147, 6050-6139`). Builds the symbol tables. [medium]
  decls.rs          — check_struct/class/enum/impl blocks, type-ref validation, param/field
                      defaults, require_signature (`:3078-3475`). [medium]
  traits.rs         — trait impl/coherence/derives/orderable/serializable + satisfies/bounds
                      (`:3475-4027, 6502-6743`). Self-contained rule cluster. [medium]
  effects.rs        — the coloring rules: check_await_positions, yield/async/spawn/isolate-Send
                      (`:2853-3078, 7476-7533`). Reads only the coloring fields. [medium]
  expr/
    core.rs         — THE BIDIRECTIONAL CORE, kept together: check/check_inner/try_adapt_literal/
                      assignable/subsume/synth/synth_inner dispatch (`:4027-5140`). Do not split
                      synth_inner by arm-groups beyond delegating to the siblings below.
    ops.rs          — synth_binary/unary, operator-trait errors, width rules (`:5250-5478`).
    calls.rs        — synth_call*, generic instantiation/bounds, closure-arg finalization,
                      check_args (`:5500-6050`).
    member.rs       — synth_member/field_set, privacy, namespaces, bundle dispatch (`:5140-5250,
                      6689-7021`).
    patterns.rs     — synth_match, exhaustiveness, bind_pattern/payload_types (`:7021-7250`).
  subst.rs          — erase_type_params/bind_type_params/apply_subst/qualify_externs/from_ref_q
                      and the other free helpers (`:7250-8115`). [easy]
  (existing)        — tiers.rs, stdlib.rs, packed.rs, attributes.rs stay as-is; move
                      check_tier_decls/check_semantic_roles (`:6139-6502`) into tiers.rs.
```

Two structural improvements worth doing during (not after) the split: (1) group `Checker`'s fields into `Symbols`, `Imports`, `Coloring`, `Config` sub-structs so each module's borrow surface is explicit; (2) route the ~10 stray `diags.push` sites through `error()` first (Finding 11) so the funnel survives the move. The bidirectional core (`expr/core.rs`) stays one file on purpose — `check ↔ synth` mutual recursion is the algorithm, and splitting it would trade one long file for hidden coupling.
