# VM & Runtime Core — Architectural Audit (noeta-vm, noeta-value, noeta-gc, noeta-bytecode, noeta-jit(-abi), noeta-runtime, noeta-object, noeta-aot-runtime)

Audit basis: full structural read of `crates/noeta-vm/src/lib.rs` (10,685 lines) and its sibling modules, plus targeted deep-reads of the other eight crates. All file:line references are against `/home/niklas/Code/lang/.claude/worktrees/audit-main`. Where a finding challenges a documented decision, that is stated explicitly.

---

## 1. The VM god-file regrew past its own tracked split — the decomposition has no ratchet

**Severity: high**

**Evidence:** `plans/code-quality/split-vm-lib.md:3-8` records the split as *"Status: done (core split)"* with *"lib.rs dropped 7733 → 5729 LOC"* and a goal of *"no module over ~1500 LOC"*. Today `crates/noeta-vm/src/lib.rs` is 10,685 lines — 8,360 non-test (tests occupy 8,361–10,685). The regrowth is attributable to whole subsystems that landed in lib.rs after the split: the tier-1 runtime glue (`lib.rs:1349–2060` — `extern "C"` JIT helpers, `frame_layout`, `PreparedCall`, `compile_module_aot`), JIT engine management (`init_jit`/`init_jit_service`/`jit_enter`/`jit_osr_backedge`/`bind_aot_dispatch`, `lib.rs:2432–2853`), the hot-swap/fragment cluster (`apply_pending_hotswap` through `debug_set_variable`, `lib.rs:6586–7130`), `run_isolate_worker` (`lib.rs:3076–3188`), and a steadily growing `VmBackend` entry-point family (`lib.rs:383–906`, ~15 `run_module_*` variants).

**Why it matters:** SRP at file granularity. The split plan proved the pattern works (`methods.rs`, `scheduler.rs`, `values.rs` were moved verbatim with the differential unchanged), but nothing prevents each new arc from defaulting into lib.rs — five arcs did exactly that. The plan's own follow-on notes ("the `Vm` 25-field regrouping remains available") are now stale: the struct has ~66 fields (see Finding 3).

**Proposed remedy:** Re-run the proven verbatim-move pattern on the post-split accretions (concrete module list in the decomposition sketch at the end). Then add a ratchet: a line-count check in CI or a standing note in `plans/code-quality/` that new subsystems land in their own module. Do **not** split the `dispatch` match itself — that was assessed and declined for documented perf/cohesion reasons (`split-vm-lib.md:13-16`), and this audit concurs.

**Perf-regression risk: none.** Same-crate module moves; all prior extractions were oracle-verified byte-identical.

---

## 2. Bytecode op semantics exist in three hand-synced copies; parity is enforced only by a runtime oracle

**Severity: high** (challenging a documented decision — partially)

**Evidence:**
- Tier-0 SSOT: `noeta-value/src/ops.rs:23` `apply_binary` (the interpreter delegates: `noeta-vm/src/lib.rs:6236`).
- Copy 2: the JIT re-emits int/float semantics in Cranelift IR — `noeta-jit/src/lib.rs:3457` `emit_int_binary_raw`, `:3581` `emit_wide_int`, `:3721` `emit_float_binary`; parity is asserted only in comments ("Bail on a zero divisor (the interpreter raises E0008)", `:3480`).
- Copy 3: `jit_run_leaf_op` in `noeta-vm/src/lib.rs:1844–2040` reproduces the happy path of 9 collection ops verbatim from the interpreter arms — e.g. `MakeRange` at `lib.rs:1859` vs the interpreter arm at `lib.rs:4287` is copy-identical logic.
- The only *shared* source of truth is bit-level, not semantic: `Value::NANBOX` ("the single source of truth so native codegen can't drift", `noeta-jit-abi/src/lib.rs:29`). Semantic sync rests on `--jit-differential` (`noeta-conformance/src/jit_differential.rs:1-12`: byte-identical output + zero leaks + zero refcount anomalies) plus in-crate tests.

**Why it matters:** DRY at the semantic level. This is inherent to tiering (the JIT *cannot* call `apply_binary` on the fast path — that is the point), and the oracle is a genuinely strong gate. But three copies with comment-only cross-references means a semantics change (e.g. a new overflow rule) must be found and applied in three places, and the oracle only catches divergence *exercised by the corpus*. The `jit_run_leaf_op` copy is the least defensible of the three: it is same-language, same-crate duplication of interpreter arms.

**Proposed remedy:** (a) For the 9 leaf ops, extract the shared happy-path computation into `#[inline]` free functions both the interpreter arm and `jit_run_leaf_op` call (the bail-vs-`Err` difference stays at the call sites — the "no register write before early return" invariant documented at `lib.rs:1835-1838` is preserved because the helpers stay pure-compute). (b) For the Cranelift copies, accept the duplication but make the sync contract explicit: a comment-anchor convention (each `emit_*` names the exact `ops.rs` function it mirrors) and a corpus-coverage assertion that every op in `is_fast_op` is exercised by at least one `--jit-differential` program.

**Perf-regression risk: low** for (a) — `#[inline]` free functions fold back; verify with the existing VM benches. None for (b).

---

## 3. The `Vm` struct is a ~66-field god-bag context

**Severity: medium**

**Evidence:** `noeta-vm/src/lib.rs:1045–1339` — the struct spans ~295 lines and ~66 fields covering at least nine concerns: derived module tables (`shapes`/`packed_schemas`/`type_reprs`/`map_packed`/`methods`/`destructors`/…, :1074–1107), globals (:1114–1118), host/executor (:1123–1129), async scheduler state (`scopes`/`ctx_current`/`traced_futures`/`channels`, :1134–1160), the extension bridge (`ext_arena`/`ext_state`/`ext_closed_gates`/`ctx_table_pool`/`embed_handles`, :1161–1188), real-thread isolates (:1197–1210), ~18 `#[cfg(feature = "jit")]` tier-1 fields (:1220–1316), debug/hot-swap (`debug_session`/`hot_mailbox`/`applied_swaps`/`pure_eval`/`debugger`/`profiler`, :1052–1071, :1317–1323), and run output (`stdout`/`diagnostics`/`requested_exit`, :1211–1216). `split-vm-lib.md:19-20` deferred regrouping when the struct had 25 fields ("remains available if a future change motivates it") — it has since ~2.6×'d.

**Why it matters:** This is the "context struct became a god-bag" pattern from the mandate. Every subsystem method takes `&mut self` and therefore conceptually touches all 66 fields; the compiler cannot enforce that the scheduler doesn't poke JIT state. It also directly causes Finding 4 (manual state mirroring) — and the motivating "future change" the plan waited for has already happened: `SessionState` independently re-derived one of the natural groupings.

**Proposed remedy:** Group into embedded plain structs with the seams the code already exhibits: `persist: SessionState` (see Finding 4), `tier1: Tier1State` (all cfg-jit fields — collapses ~18 `#[cfg]` attributes into one), `isolates: IsolateState`, `sched: SchedState`, `out: RunOutput`. Field access becomes `self.tier1.jit_counters` — same machine code (flattened offsets), and borrow-splitting *improves* (disjoint `&mut self.tier1` / `&self.module` borrows).

**Perf-regression risk: none.** Struct nesting compiles to identical offset arithmetic; no indirection is introduced.

---

## 4. `SessionState` manually mirrors 16 `Vm` fields across four hand-maintained sites

**Severity: medium**

**Evidence:** `noeta-vm/src/session.rs:39-68` (`SessionState`, 16 fields), `:72-91` (`fresh`), `:141-165` (`load_seeded` — 15 field-by-field copies plus a rebuilt `map_packed`), `:170-189` (`into_state` — the same 16 back out). The doc at `:139-140` acknowledges the coupling: "it keeps `load_seeded` in lockstep with `load`'s field init".

**Why it matters:** Adding one new piece of persistent runtime state to `Vm` requires touching four locations; forgetting `load_seeded`/`into_state` silently drops REPL/embed/hot-swap state between entries — a corruption class no compiler error or leak-oracle catches (the state isn't leaked, it's *reset*). This is the sharpest concrete cost of Finding 3.

**Proposed remedy:** Make `SessionState` an embedded field of `Vm` (`vm.persist`). `load_seeded` becomes `vm.persist = state` (one move), `into_state` becomes `self.persist`. The `map_packed` rebuild stays as the one bespoke step.

**Perf-regression risk: none.** Cold path (session entry/exit); field grouping is free per Finding 3.

---

## 5. Every re-entrant closure application allocates a full run context — per element on the iterator/map paths

**Severity: medium** (perf + architecture)

**Evidence:** Builtin higher-order paths call `call_value` once per element: `noeta-vm/src/methods.rs:520,534,556,611,659` (`iter_next_apply` drains: `Next`/`Collect`/`Count`/…) and `lib.rs:8044,8087,8123,8152` (eager `map`/`filter`). Each `call_value` (`lib.rs:7131`) builds `vec![Value::unit(); num_registers]` + a `Vec<Frame>`, then `run` (`lib.rs:3539`) executes `#[cfg(feature = "jit")] regs.reserve(8192…)` (`:3546` — a 64 KB capacity allocation per call in production, where `jit` is on), and `dispatch` allocates **two** cache vectors sized to the *whole module's* cache-slot count: `vec![None; self.module.cache_slots as usize]` twice (`lib.rs:3617-3624`). So one `xs.map(f)` over N elements performs ~5 allocations per element, two of them O(program size) zeroed.

**Why it matters:** The cache-vector locality is documented ("A local (not a `self` field) so it neither borrows `self` in the loop nor leaks across runs", `:3614-3616`) — but the doc justifies the *borrow* choice, not the re-entrancy cost, which appears unexamined. The codebase already pools exactly this shape of scratch elsewhere (`ctx_table_pool`, `lib.rs:1185-1188`: "a ctx dispatch pops one instead of allocating"), so this is an inconsistency with its own established pattern, not a novel idea.

**Proposed remedy:** Pool the re-entrant run context on `Vm` (a stack of `(Vec<Frame>, Vec<Value>, caches, extern_caches)` popped by `call_value`/`run_thunk`, cleared and returned on exit — re-entrancy nests, so a stack suffices, exactly like `ctx_table_pool`). Gate the 8192-register reserve to the outermost run only.

**Perf-regression risk: none — this is a perf improvement.** Bench with an iterator-heavy program before/after; the differential oracle gates semantics.

---

## 6. The callee-frame-push protocol is duplicated at least four times inside `dispatch`

**Severity: medium**

**Evidence:** The sequence *arity-range check → `reserve_window` → retain receiver/args into the window → run default thunks → `frames[top].pc = pc + 1` → `frames.push(Frame{…})` → `continue 'reload`* appears near-verbatim in: the object method call (`lib.rs:4767-4812`), the enum method call (`lib.rs:4834-4870` — ~37 lines literally identical to the object copy), `Op::Invoke` (`lib.rs:5980-6023`), and the `Op::Binary` operator-overload dispatch (`lib.rs:6186-6205`); `setup_closure_call` (`lib.rs:7742`) and `call_value` (`lib.rs:7150-7182`) carry two more variants. 11 `frames.push(Frame` sites exist across the crate.

**Why it matters:** DRY on a correctness-critical protocol: the retain/default-thunk/`ret_transform` choreography is exactly where refcount bugs live (the codebase's own `native_ctx.rs:1-7` header records that hand-rolled retain choreography "leaked twice during the http-server arc"). A future change to frame setup (e.g. a new `RetTransform`) must find every copy.

**Proposed remedy:** Extract an `#[inline]` `fn push_callee_frame(&mut self, frames, regs, top, proto, recv: Option<Value>, args: &[…], ret_dst, transform, resume_pc) -> Result<(), Abort>`; each arm keeps its own `continue 'reload`. This is precisely the shape of the already-benched `call_builtin_method` extraction (`lib.rs:7253-7256` documents the `#[inline]` fold-back verified at ±0).

**Perf-regression risk: low.** `#[inline]` + monomorphic call sites; verify with the existing `benches/vm.rs` call benches, as was done for `call_builtin_method`.

---

## 7. User-method lookup allocates two `String`s per miss, and enums/operators bypass the inline cache entirely

**Severity: medium**

**Evidence:** `methods: HashMap<(String, String), u32>` (`lib.rs:1089`). The object path amortizes it through the shape-pointer inline cache (`lib.rs:4733-4765`), but the enum path is documented as uncached — "Enums carry no inline-cache shape pointer, so this is a direct table lookup" (`lib.rs:4814-4817`) — and performs `v.shape().unwrap().name.clone()` + `method.to_string()` per call (`:4819, :4832`). `Op::Binary`'s operator-overload dispatch does the same two allocations per executed operator on object/enum operands (`lib.rs:6160-6177`), and `Op::Invoke` likewise (`:5951`).

**Why it matters:** Two heap allocations per dynamic dispatch on paths users will consider hot (enum methods, overloaded operators on value types). The enum comment states the fact but offers no rationale — the op already carries a `cache` slot (`:4567`), and enums carry a shape pointer via `v.shape()`, so the object IC appears directly applicable.

**Proposed remedy:** (a) Extend the shape-pointer inline cache to the enum arm (same `caches[ci]` slot, same hit test). (b) Longer-term, key the method table by `(shape index, NameId)` — the module already interns names (`module.name(*method)` returns `&str` from a table) — eliminating owned-String keys everywhere including `Op::Binary`'s `op.overload_method()` lookups.

**Perf-regression risk: none — strictly removes work.** IC-hit semantics are already proven on the object path.

---

## 8. `noeta-value/src/lib.rs` is a second god-file: ~180 methods in one `impl Value`

**Severity: medium**

**Evidence:** 4,329 lines, of which tests run from `lib.rs:2958` to EOF; the ~2,950-line body is one `impl Value` holding: the NaN-box codec (:104–250), packed-list machinery (~360 lines, :409–770), the iterator engine (~500 lines, :794–1311, including a ~180-line `iter_next_apply`), concurrency value kinds (:868–1056), display/JSON/marshalling (~270 lines, :2257–2523), reflection tags, and the refcount/GC bridge (:2620–2784). The crate already demonstrates the right split (`heap.rs` = the sole unsafe representation module, `ops.rs` = operators, `ids.rs` = newtypes).

**Why it matters:** SRP; same trajectory as noeta-vm's lib.rs. The `Payload`/`heap::with_payload` seam already isolates these method groups from the encoding, so cohesion does not require colocation.

**Proposed remedy:** Extract `packed.rs`, `iter.rs`, `display.rs`, `conc.rs` as additional `impl Value` blocks (same-crate, verbatim moves — private access is preserved). Core lib.rs retains codec + basic constructors/accessors + refcount bridge (~1,200 lines).

**Perf-regression risk: none.** Same-crate `impl` blocks are layout- and inlining-neutral.

---

## 9. The VM↔eval backend mirror (~2,000 LOC) is acknowledged in prose but tracked nowhere

**Severity: medium** (process/ledger finding)

**Evidence:** `ARCHITECTURE.md:115`: "Routing/dispatch that is still mirrored between the backends is a known debt tracked in `plans/`" — but no `plans/` file scopes it (the code-quality tracks cover *file splitting* only). The mirror is real and large: `noeta-vm/src/methods.rs` (1,066 LOC) ↔ `noeta-eval/src/lib.rs:2517-3400` (~900 LOC, ":2513 'Mirrors the VM's `call_list_method`'"); `scheduler.rs` (667 LOC) ↔ eval's poll/scope cluster (eval `:1148` "both round-robin identically"); `narrow_matches` (`noeta-vm/src/lib.rs:2166`) ↔ eval `:4587-4635` (":4612 'mirrors the VM's narrow_matches'"). `noeta-eval` carries 74 explicit "mirror" comments. By contrast the method-name *vocabulary* (exhaustive `ListMethod`/`MapMethod`/… enums) and all Ring-2/native dispatch (`noeta_stdlib::registry::dispatch_method`, `registry.rs:672`) genuinely live once.

**Why it matters:** The duplication itself is a deliberate, oracle-gated design invariant (two backends must not share a value model) and this audit does **not** recommend unifying it. The finding is narrower: the architecture document claims the debt is tracked, and it is not — so there is no recorded assessment of which mirrored pieces (e.g. the channel FIFO, the scheduler round-robin *policy*) could be lifted into `noeta-stdlib` as value-model-neutral logic the way the method enums were.

**Proposed remedy:** Write the missing `plans/` doc: inventory the mirror, classify each piece as "irreducible (touches value representation)" vs "liftable policy", and either scope the liftable subset or record the decision not to.

**Perf-regression risk: none** (documentation), low for any later lift (oracle-gated).

---

## 10. The 11-entry JIT helper table is built twice, verbatim

**Severity: medium-low**

**Evidence:** `noeta-vm/src/lib.rs:2442-2466` (`init_jit`, `&[(&str, *const u8)]`) vs `:2495-2532` (`init_jit_service`, the same 11 `(name, ptr)` pairs as `usize`). Adding a helper today means editing both lists; a missed edit fails only at JIT-time symbol resolution.

**Why it matters:** DRY on an ABI-critical table. The rest of the sync/service duality is well-deduplicated (the mirror tables `jit_entries`/`jit_fast` are the documented single lookup source, `lib.rs:1278-1285`, with `jit_install` the single writer), which makes this residue stand out.

**Proposed remedy:** One `fn jit_helpers() -> [(&'static str, *const u8); 11]`; each init maps to its pointer representation.

**Perf-regression risk: none.** Startup-only.

---

## 11. `noeta-jit/src/lib.rs` is a third god-file, with ~480 lines of pure analysis stranded next to codegen

**Severity: medium-low**

**Evidence:** 4,248 lines. `plan.rs` (551 lines) already extracted the register liveness/residency analysis cleanly, but the same *kind* of pure analysis remains in lib.rs: `heap_in_map`, `proto_modeled`, `slot_hazard_map`, `transfer_pairs`, `heap_at_fixpoint`, `kind_in_map`, `must_slot_written_map`, `fast_ok` (`noeta-jit/src/lib.rs:1526-2007`). `from_module` (`:223-435`) is ~210 lines of signature-building boilerplate; `emit_call` (`:2821-3105`) is the largest single function at ~284 lines.

**Why it matters:** Same SRP trajectory; `plan.rs` proves these analyses are pure and independently testable.

**Proposed remedy:** Move the `heap_*`/`kind_*`/`slot_hazard` cluster into `plan.rs` (or a sibling `analysis.rs`); table-drive the 11-import signature boilerplate in `from_module`.

**Perf-regression risk: none.** Compile-time-of-the-JIT only; codegen output unchanged.

---

## 12. `noeta-value` depends on `noeta-stdlib`, making the value crate non-layer-minimal

**Severity: low** (documented-adjacent; flagged as a coupling cost, not a violation)

**Evidence:** `noeta-value/Cargo.toml:24` `noeta-stdlib = { path = "../noeta-stdlib", default-features = false }` (for `MapKey`, `ExternValue`, `NativeValue`). No cycle exists (`noeta-stdlib` does not depend back), but `noeta-stdlib` transitively pulls `noeta-native`/`noeta-reactive`/`noeta-crdt`, so the "one 64-bit word" crate rebuilds whenever the extern/reactive/crdt surface changes. The crate-map framing in `ARCHITECTURE.md:74-79` (value model as a bottom layer) does not mention this upward edge.

**Why it matters:** Build-graph coupling and conceptual layering honesty; the extern-value *contract* is the only thing needed, not the stdlib.

**Proposed remedy:** If it ever bites (rebuild times, embed builds), extract the `ExternValue`/`MapKey`/`NativeValue` vocabulary into a leaf contract crate both depend on. Until then, document the edge in ARCHITECTURE.md's crate map.

**Perf-regression risk: none.**

---

## 13. unsafe/documentation hygiene residue

**Severity: low**

**Evidence (three independent items, all verified):**
1. Stale SAFETY comment on the one transmute in noeta-jit: `noeta-jit/src/lib.rs:955-957` describes the finalized entry as `extern "C" fn(ptr, ptr, usize) -> u32`, but `CompiledFn` (`noeta-jit-abi/src/lib.rs:79-87`) takes 7 params and returns `i64`. The transmute target type is correct; the load-bearing justification prose is wrong.
2. ~16 unsafe derefs in the cycle-collector primitive accessors (`noeta-value/src/heap.rs:779-1028`: `color`/`set_color`/`rc_inc`/`children`/…) carry no per-block SAFETY comment, covered only by a section header at `:811-815` — inconsistent with the same file's convention elsewhere (`:576-578, :926-928` etc.).
3. Orphaned doc-comment in noeta-vm: the 22-line doc for `install_fragment` (`lib.rs:6551-6572`) is immediately followed by `apply_pending_hotswap`'s own doc (`:6573-6585`), so both blocks attach to `apply_pending_hotswap`; `install_fragment` itself (`:6670`) is undocumented — drift from a code move. Related: "miri-gated" in `noeta-gc/src/lib.rs:6` means *miri-covered under `cargo miri test`*, not `#[cfg(miri)]`.

**Why it matters:** SAFETY comments are the audit trail for the workspace's unsafe-quarantine policy (`ARCHITECTURE.md:120`); a stale one is worse than none.

**Proposed remedy:** Fix the `:956` signature prose; add one-line SAFETY notes (or a `// SAFETY: see module invariant above` pointer) to the heap.rs accessor cluster; re-home the `install_fragment` doc.

**Perf-regression risk: none.**

---

## 14. Minor consistency residue: stringly-typed JIT errors; glob re-imports in extracted modules; entry-point proliferation

**Severity: low**

**Evidence:**
- The entire noeta-jit compile API returns `Result<_, String>` (`noeta-jit/src/lib.rs:182, 227, 435, 905, 947`), against the workspace's typed-diagnostics discipline — defensible since every call site discards the error into "decline and keep interpreting" (`noeta-vm/src/lib.rs:2731`), so the `String` never carries information anywhere.
- The extracted modules import via `use crate::*;` (`methods.rs:12`, `values.rs:17`, `scheduler.rs:14`), so the module boundaries are physical, not logical — documented as the verbatim-move pattern ("moved verbatim… purely to shrink lib.rs", `methods.rs:1-6`).
- `VmBackend` has ~15 `run_module_*` variants (`lib.rs:383-906`) forming a host × executor × jit-mode × debugger × session × stats matrix; each is individually documented but the family grows one method per new mode combination.

**Proposed remedy:** Opportunistic only: a zero-size `JitDecline` error type; narrow the glob imports when files are next touched; collapse the entry family onto a `RunOptions` struct with 3–4 thin conveniences kept for the differential/oracle call sites.

**Perf-regression risk: none.** All cold/setup paths.

---

## What's already good (seams worth preserving)

- **The Debugger/ProfileHook seams** — one `Option` check (a predicted branch) per op, documented and A/B-benched (`lib.rs:1317-1323, 3733-3761`); the debugger take-out-of-`self` dance during a pause is an elegant re-entrancy solution.
- **The differential-oracle architecture** — VM↔eval byte-identity, plus the separate `--jit-differential` gate (output identity + zero leaks + zero refcount anomalies) covering what miri cannot (generated native code).
- **NaN-box encapsulation** — `pub struct Value(pub(crate) u64)` (`noeta-value/src/lib.rs:133`); the only raw surfaces are `from_bits`/`bits` (opaque tokens at the ABI/identity boundaries) and `Value::NANBOX` as the *deliberate* JIT bit-contract. `heap.rs` is verifiably the sole unsafe module in the crate; strict-provenance (`expose_provenance`) done correctly.
- **The tier-1 mirror tables** — `jit_entries`/`jit_fast` as the single lookup source for sync, service, and AOT modes, with `jit_install` the single writer.
- **`noeta-jit-abi`** — a genuinely frozen, cranelift-free, unsafe-free ABI vocabulary crate; the JIT touches VM state only through `FrameLayout` offsets and opaque pointers.
- **`NativeCtx`** — the god-trait split is executed, not aspirational: capability broker (`noeta-native/src/ctx.rs:265`) + `TaskContext`/`FutureTracing`/`HotReload` sub-traits (`:360-378`); `VmCtx`'s slot table centralizes retain/release ownership after two documented leak incidents.
- **`noeta-bytecode`** — pure data as claimed, with the `Op` cache-line budget *enforced by a test* (`tests/op_size.rs`), not just documented.
- **Error handling in the VM** — one `error()` chokepoint producing typed diagnostics + a zero-size `Abort` unwind token; the abort traceback is written only after an abort (zero hot cost).
- **The two-loop `'reload` dispatch structure**, dispatch-local inline caches, `ArgBuf` inline staging, and `ctx_table_pool` — all documented, benched hot-path engineering.
- **Feature layering** (`compile`/`jit`/`jit-rt`/`aot`) — clean, documented, and lets AOT binaries shed the compiler; `noeta-aot-runtime` is 150 lines and clean.
- **The extraction pattern itself** — `methods.rs`/`scheduler.rs`/`values.rs`/`session.rs`/`isolate.rs`/`debug.rs`/`jit_service.rs` prove verbatim `impl Vm` moves are safe and cheap here.

---

## Proposed decomposition sketch for `noeta-vm/src/lib.rs`

Target: lib.rs ≈ 600 lines; no new module over ~1,500 except `dispatch.rs` (kept intact per the documented assessment). All moves are same-crate verbatim relocations — the proven zero-risk pattern.

| Module | Responsibility | Source lines (today) | ~LOC |
|---|---|---|---|
| `lib.rs` (kept) | Crate docs, module decls/re-exports, `Vm` struct (regrouped per Finding 3), `Frame`/`RetTransform`/`Abort`, constants | 1–110, 916–1348 | ~600 |
| `hooks.rs` | `Debugger` + `ProfileHook` traits, `DebugAction`/`DebugEvalRequest`/`DebugSetRequest`, `DebugView`/`DebugFrame`, `EvalBudget` | 82–380 | ~350 |
| `backend.rs` | `VmBackend` + entry-point family (collapsed onto `RunOptions`), `execute*`, `run_and_teardown`, `JitStats`/`JitReport` types | 383–915, 2222–2260, 2854–2941 | ~650 |
| `tier1.rs` (cfg jit/jit-rt) | `extern "C"` helper fns, `PreparedCall`, `frame_layout`/`fresh_frame_template`/`vec_header_words`, `compile_module_aot`, `JitOutcome`, `init_jit`/`init_jit_service` (shared helper table per Finding 10), `jit_enter`/`jit_osr_backedge`/`jit_install`/`bind_aot_dispatch` | 1349–2060, 2432–2853 | ~1,450 |
| `lifecycle.rs` | `Vm::load`, `teardown`, `reclaim_cycle_garbage`, `release_value`, `run_destructor`, `run_isolate_worker` + `IsolateSlot`/`IsolateFactory`/`Task`/`Channel` decls (or fold into `scheduler.rs`) | 2047–2221, 2266–2431, 2942–3538 | ~900 |
| `dispatch.rs` | `run` + the `dispatch` loop **intact**, `set_reg`/`reserve_window`/`ArgBuf`; gains the `push_callee_frame` helper (Finding 6) | 3539–6549, 8299–8360 | ~3,100 |
| `hotswap.rs` (cfg compile) | `FragmentCompiler`/`HotFragment`/`HotChannel`, `apply_pending_hotswap`/`apply_one_swap`/`install_fragment`/`hotswap_retire_tier1`/`hotswap_rearm_tier1`, fragment eval (`debug_eval_fragment`/`compile_fragment_entry`/`run_installed_fragment*`/`debug_set_variable`) | 149–217, 6550–7130 | ~900 |
| `calls.rs` | `call_value`, `run_method_handle`, `run_thunk`, `setup_closure_call`, `do_return*`, `call_native_fn`, `call_builtin`, `check_arity` (keep `#[inline]` on `call_builtin_method` wherever it lands — natural home is `methods.rs`) | 7131–8298 | ~1,150 |
| `tests/` or `src/tests/*.rs` | The existing unit-test module, split by subject | 8361–10685 | ~2,300 |

Hot-path constraints honored by the sketch: the `dispatch` match stays one function (jump-table codegen, per `split-vm-lib.md:13-16`); nothing on the per-op path gains a trait object or a new `Option` check; extractions that touch per-call paths (`push_callee_frame`, `call_builtin_method`'s home) stay `#[inline]` and get re-verified against `benches/vm.rs` — the same protocol the `call_builtin_method` extraction already established.
