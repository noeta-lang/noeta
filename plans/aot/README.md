# AOT & bundling arc — source-free binaries (P-AOT)

**Status: Levels 1 & 2 COMPLETE (branch `aot-bundling`, unmerged); Level 3 not started.** Goal:
ship a runnable artifact that does **not** include the `.noe` source. Three levels of increasing
ambition and cost — bytecode bundle → self-contained executable → native AOT + DCE. Levels 1–2
deliver the source-hiding goal cheaply and hand Level 3 its interpreter-fallback substrate for free;
Level 3 is the performance/opacity milestone. **Delivered: `noeta build` → obfuscated `.noeb`
(`noeta run app.noeb`), and `noeta build --exe` → a single self-contained executable — both source-
free, differential-proven identical to a source run.**

Provenance: design conversation 2026-07-07 (after the P-JCT JIT compile-throughput arc). The user
confirmed **JIT op-coverage expansion is deferred to a separate later track** — AOT ships at
today's coverage via the interpreter fallback; coverage is shared work that lifts both tiers and is
sequenced independently (see *Out of scope*).

## What exists today (verified against the repo, 2026-07-07)

- **The pipeline recompiles from source every run.** `noeta-cli::compile_real` (`main.rs:226`)
  lexes/parses/checks/compiles a `.noe` into a `noeta_bytecode::Module`; `cmd_run` (`main.rs:316`)
  loads it onto the VM. There is **no `build`/`compile`/`bundle` command** — only
  run/test/bench/doc/dump/repl/lsp/dap.
- **`Module` is plain owned data.** `Module` (`noeta-bytecode/src/lib.rs:1190`) holds `Vec<Chunk>`,
  `Vec<Shape>`, constants, method/destructor/derive tables, `reflection: ReflectionInfo`, and R1
  `TypeRepr` tables. A scan for `Rc`/`Arc`/`*const`/`*mut`/`Box<dyn>`/fn-pointers across
  `noeta-bytecode`, `noeta-object` (`Shape`), and `noeta-ast::reflect` (`ReflectionInfo`,
  `TypeRepr`) finds **none** — every constituent is serde-derivable with a shallow sweep.
  `noeta-span` (`Span`/`SourceId`/`Source`) **already** derives `Serialize`/`Deserialize`.
- **The interned-`&'static Shape` handles are a VM-load-time concern, not a Module field.** `Module`
  stores owned `Vec<Shape>`; the VM/JIT interns them (`noeta_object::intern_shape`) *when it loads a
  Module*. So a deserialized Module re-interns on the existing load path — serialization sidesteps
  the leaked-pointer wrinkle entirely. (This corrects the initial design-chat framing, which
  over-weighted it.)
- **The JIT is already a hybrid** — native for ~a quarter of the 125 `Op` variants (`is_fast_op`:
  arithmetic/comparison `Binary`, `Move`/`Drop`, globals, `Call`/`Return`, branches, ~9 leaf heap
  ops), and **every uncovered op bails to the interpreter mid-frame and resumes native** via
  resume-pcs. AOT inherits this model: it does **not** need full coverage to function; it embeds the
  interpreter as the fallback, exactly as the JIT does at runtime.
- **`noeta.toml` build profiles already parse** (`noeta-cli::manifest`), resolving a profile to an
  active-tier set. A production build naturally strips `@test`/`@debug`/`@doc` tier blocks *before
  lowering* (they never reach the Module), so a bundled artifact excludes that content by
  construction — no DCE needed for tier content.
- **The M3 "startup cache" roadmap item is subsumed** by Level 1 (a cached serialized Module *is* a
  startup cache); this arc delivers it as a side effect.

## Posture (inherited from the perf/M2 arcs)

- **New differential oracle: bundle-run ≡ source-run, byte-identical.** Every corpus program run
  from its serialized/AOT artifact must produce a `RunResult` identical to running it from source.
  This is the safety spine — the existing `SandboxHost` determinism makes it well-defined. Wired
  into conformance so `0 skipped` still holds.
- Determinism unchanged (seeded RNG, logical clock, sorted iteration). No wall-clock in output.
- Gates per slice: conformance, both differentials (backend + bundle), `cargo test --workspace`,
  clippy, fmt. Any codegen slice (Level 3) also re-runs the jit-differential and leak oracle.
- **Commit per green slice; never push without authorization.** Work in a dedicated branch/worktree.

## Level 1 — bytecode bundle (the cheap win: no `.noe` shipped)

Serialize the compiled `Module`; ship the blob; run it directly, skipping the front-end.

| # | Slice | Depends | Notes |
|---|---|---|---|
| L1.0 | serde the bytecode graph | — | ✅ **DONE** (`2a3ed41`). Serde sweep over the whole graph (Op/Chunk/Const/Module, Shape/ShapeKind, ReflectionInfo/TypeRepr/AttrArg/AttrValue/BinaryOp/UnaryOp, IntMethod/TypeRecipe) — all plain owned data; `Module` keeps owned `Vec<Shape>`, the interned `&'static` runtime types (`noeta_object::PackedSchema`/`PackedKind`) are *not* reachable and stay non-serde. `Module::encode`/`decode` via postcard. Corpus round-trip oracle: 494 modules byte-stable, 0 skipped. |
| L1.1 | versioned `.noeb` artifact format | L1.0 | ✅ **DONE** (`92091bd`). New `noeta-bundle` crate (isolated from the mid-end so future compression/crypto stays out of core): `[magic NOEB | fmt_ver | flags | rt_len | rt_ver | payload]`; `rt_ver` pins artifacts to their builder (mismatch = clear error); `flags` reserves compressed/encrypted bits (a set bit is rejected in v1). `write`/`read`/`is_bundle`. |
| L1.2 | `noeta build <file> -o app.noeb` | L1.1 | ✅ **DONE** (`57ec769`). `noeta build` compiles via `compile_real` (tiers stripped unless `--tier`/`--profile`), serializes. `noeta run app.noeb` sniffs the magic and runs the module directly — no source, no compile/check; build-time flags rejected. Aborts render against a synthetic empty source (message/code/trace show, no snippet). 2 CLI integration tests. |
| L1.3 | bundle differential oracle | L1.2 | ✅ **DONE** (`f7047a1`). Beyond L1.0's byte round-trip, the *decoded* module runs byte-identically to the source-compiled one on the sandbox (stdout/exit/diagnostics). 494 modules round-trip AND run identically; 0 skipped. |
| L1.4 | **obfuscation (default, no key)** | L1.1 + L1.3 | ✅ **DONE** (`795d7ab`). Default payload = deflate (miniz_oxide, pure-Rust) + fixed-seed SplitMix64 XOR scramble (`FLAG_COMPRESSED`). Not plaintext, string constants absent, `noeta dump` can't read it; no key, zero distribution friction. Honestly obfuscation, not security. Chose deflate over zstd for the pure-Rust posture (one-line swap later). |
| ~~L1.5~~ | ~~optional keyed encryption~~ | — | **CUT (user, 2026-07-07), on principle — see below.** Access control / licensing is *policy*, which belongs to the developer's application, not the build tool. Encryption's only non-redundant capability (protect the artifact *at rest* before execution) is marginal, fragile, and itself buildable by a developer on the crypto/network primitives the language already ships. The tooling provides **mechanism** (obfuscation, native-AOT opacity, crypto/network stdlib); the developer builds **policy**. |

**Outcome of Level 1:** a `.noeb` you ship instead of `.noe`. By default it is **obfuscated**
(compressed + scrambled — not human-readable, not `noeta dump`-able, no key to manage);
Also a startup-cache win: skips the whole front-end.

## Security model — obfuscation by default; access control is the developer's, not the tool's

**Decision (user, 2026-07-07): the tool provides *mechanism*; the developer provides *policy*.**
The build tool obfuscates by default (no key, no friction) and — via native AOT (Level 3) — makes
the covered code opaque. It does **not** ship an access-control / licensing / encrypt-at-rest layer,
because:

- **Obfuscation (L1.4, shipped) is the right default.** The artifact is deflate-compressed +
  scrambled: not plaintext, not `strings`-able, not `noeta dump`-able, defeats casual inspection and
  automated tooling. **Zero distribution friction.** Honestly labeled obfuscation, not security —
  the de-obfuscation algorithm is in this open-source runtime, and the `Module` is recoverable from
  process memory at run time. That is the accepted, stated bar for the default. Native AOT raises the
  opacity bar further for the covered subset.
- **Keyed encryption was considered and cut.** Its *only* capability that obfuscation/AOT don't
  already provide is protecting the artifact **at rest, before execution** ("a leaked copy is inert
  without an external key") — an **access-control** property, not an anti-reverse-engineering one.
  That is application *policy*: licensing, entitlement, distribution control. It belongs in the
  developer's program, built on the crypto/network primitives the language already ships (HMAC /
  signatures / verify / network). A developer who genuinely needs encrypt-at-rest can wrap the
  bundle themselves on those primitives; the build tool baking in one opinionated scheme — with its
  crypto deps and key-distribution burden — for a capability that is marginal (narrow deploy shape),
  fragile (a host-controlling attacker dumps the decrypted image), and mostly app-level, does not
  earn its place. So `FLAG_ENCRYPTED` stays a reserved header bit (a v1 reader rejects it), but no
  encryption layer ships here.

**Rule of thumb:** hide source / deter casual RE → obfuscation (default) + native AOT (Level 3).
Control *who may run or hold* the artifact → the developer builds that in their app; not a
`noeta build` concern.

## Level 2 — self-contained executable (one file, no separate interpreter)

| # | Slice | Depends | Notes |
|---|---|---|---|
| L2.0 ✅ | embedded-blob bootstrap | L1.2 | **DONE** (`64c9f9d`). At startup `main` reads only the tail of its own executable (`current_exe` + seek, not a slurp): a fixed 16-byte trailer `[bundle_len u64 LE | "NOEBEXE\0"]`. Sentinel present → run the embedded bundle via `cmd_run_bundle`; absent → normal CLI. Any IO/format hiccup ⇒ "no bundle" (toolchain must still start). Trailer-append, no per-OS section surgery. |
| L2.1 ✅ | `noeta build --exe -o app` | L2.0 | **DONE** (`64c9f9d`). `noeta_bundle::staple(runtime, bundle)` = `[runtime image | bundle | trailer]`; the OS still sees a valid exe. `noeta build --exe` embeds *this* binary + the obfuscated bundle (default out = input with extension stripped, `.exe` on Windows; `chmod 0o755` on Unix; refuses to clobber the source). Still bytecode under the hood (obfuscated, L1.4). 2 bundle unit + 2 CLI e2e tests. |

**Cross-compilation** (building an `app` for a different target triple) is **out of Level 2 v1** —
it ships the host-target runtime. Flagged as a later extension (needs prebuilt per-target runtime
binaries to staple onto).

## Level 3 — native AOT + DCE (the backend milestone)

Compile functions to machine code ahead of time and link a standalone binary. Reuses the JIT's
Cranelift codegen; the covered subset goes native, the rest runs through the **embedded interpreter
from Levels 1–2** (the same hybrid the JIT uses at runtime).

| # | Slice | Depends | Notes |
|---|---|---|---|
| L3.0 ✅ | generalize codegen over `cranelift-module::Module` | — (parallel to L1/L2) | **DONE** (L3.0a `4e632dd` split + genericization). `Jit<M: ClifModule = JITModule>` — the module backend is a generic param, so the *same* `emit_int_body`/`FunctionBuilder` IR construction targets both `JITModule` (runtime) and `ObjectModule` (AOT), **monomorphized → zero JIT dispatch cost**. Split `finalize` into shared `define_body → FuncId` + JIT-only `finalize_ptr`. Added `Jit::<ObjectModule>::new_object`/`compile_object`/`finish`; smoke test compiles a real program's every proto into a valid host ELF object. **Gates: jit-differential byte-identical; A/B (taskset -c 2): compile throughput 584→586 ms (+0.4%, in-noise), generated-code speed unchanged.** NOTE: a native AOT body still bakes an absolute `frame_template` pointer — running (not just emitting) AOT native bodies is L3.1. |
| L3.1a ✅ | AOT codegen mode (IC-off) + corpus oracle | L3.0 | **DONE** (`83c88f0`). **Audit finding:** the *only* absolute-address bake in the emit paths is the per-site inline-cache slot (`Box<CallSiteCache>` heap addr, line ~2714). The frame-template copy is already object-safe — it bakes the template's *words* as position-independent immediates (zeroed fields, `None` niche, empty `Vec` ptr = `align_of`), never the address. So the AOT address problem = the inline cache, nothing else. Threaded an `aot` flag into `Codegen`: an AOT body emits **no** IC hit path and passes a null slot to `prepare_call` (helper already guards `!site.is_null()`) — every call takes the always-correct helper slow path (the cold-first-call path). Oracle: `NOETA_JIT_AOT=1` makes the runtime JIT emit AOT-form bodies → jit-differential proves the AOT codegen byte-identical across the **whole corpus** before any linking. Gates green; JIT machine code unchanged (A/B +0.07%). |
| L3.1b ✅ | eager AOT compile driver | L3.1a | **DONE** (`972bf58`). `Jit::<ObjectModule>::compile_module(module) -> AotManifest`: every eligible proto → native main body + fast body (S4.1) as exported symbols; ineligible protos get no native entry (interpreted). Manifest = proto-index → symbol map (the L3.2 binding contract). Symbol names now a single source of truth (`proto_symbol`/`fast_symbol`/`stub_symbol`). Test parses the ELF symbol table and asserts every native proto symbol is a real definition. Gates green (both differential modes). |
| L3.2a ✅ | AOT dispatch-table data symbol (approach A) | L3.1b | **DONE** (`d35f2d0`). `compile_module` emits `noeta_aot_dispatch` — an exported data object, ABI `[count][main_0,fast_0,…]` (pointer-width LE words), each function slot a linker-resolved relocation to the proto body or null. The AOT object is now **complete** (bodies + dispatch wiring). Test asserts the symbol is defined + one body relocation per native main+fast entry. Runtime reads this one static (no self-dlsym). |
| L3.2b | link driver → standalone binary + runtime binding + `noeta build --native` | L3.2a + L2.1 | **The linker/build-system milestone.** Runtime reads `noeta_aot_dispatch` → `jit_install`s each entry into its mutable per-proto tables at startup (weak-symbol/absent when JIT-only). Needs a **linkable runtime archive** (the crux decision — build from source at `--native` time, or ship a prebuilt `libnoeta_runtime.a` alongside `noeta`?), a `cc`/`ld` invocation combining `[program.o + runtime archive + embedded bundle (L2-style, interpreter fallback)]` → one exe, and the `noeta build --native -o app` command. **HMR alignment (preserve):** (a) AOT calls stay indirection-routed (`prepare_call`), never baked direct native→native — a direct-call opt would be HMR-hostile, gate it; (b) keep tables runtime-mutable + interpreter/JIT live in HMR builds. |
| L3.2 | link driver → standalone binary | L3.1 + L2.1 | Link the object + the runtime + the embedded interpreter/bytecode substrate (Levels 1–2) into one binary: native for the covered ops, interpreter fallback for the rest. |
| L3.3 | AOT differential oracle | L3.2 | AOT-binary `RunResult` ≡ source-run, over the corpus. |
| L3.4 | DCE / tree-shaking | L3.2 | **Own sub-milestone, optional for shipping L3.** Whole-program reachability drops unused functions + stdlib. Hard part = dynamic dispatch (trait method tables, `invoke`, reflection `attributes_of`/`type_of`, the stdlib registry): needs conservative roots (`@reflectable`) and closes the deferred "reflection-metadata elimination" row (`deferred.md`, gated on exactly this). Aggressiveness = decision point. |

**Dev-tooling reuse & future convergence (checked 2026-07-07).** The `tooling-unification` arc is
**merged to main**, but it unified the *interactive-eval* family (LSP/DAP/REPL: one type spelling,
one fragment parser, the `SessionCompiler`/`VmSession::adopted` live-session seam, the debug console's
mid-run module swap). AOT/build is a **different axis** (codegen + linking + batch orchestration), so
it takes **no** LSP/DAP reuse and needs no shared refactor — it reuses the compile pipeline
(loader→check→compiler→bytecode) exactly as `noeta build` already does. Two *future* convergences to
keep the seams friendly toward, neither in L3.2:
- **HMR** = the intersection of the already-merged session machinery (`SessionCompiler`,
  `VmSession::adopted`, debug-console mid-run swap) *and* the AOT/JIT per-proto entry-table
  indirection (`jit_install`). HMR generalizes "extend a live session with a fragment" to "replace a
  changed module's protos + re-point the entry tables." Preserve: indirection-routed calls (no baked
  direct native→native), runtime-mutable tables, interpreter/JIT live in HMR builds.
- **Debuggable native binaries** = emit the DAP's debug-info side-tables (`Chunk::debug_lines`, the
  name map, TypeRepr rendering) as cranelift **DWARF** into the AOT object, so `noeta dap` can debug
  a native binary. L3-future ("debuggable native"); keep the manifest/symbol seam DWARF-friendly.

**Obfuscation × Level 3:** an AOT binary's **native machine code** is inherently opaque (not
bytecode — `noeta dump` can't read it), so Level 3 raises the opacity bar for free. The embedded
**bytecode-fallback blob** (the ~75% of ops the JIT doesn't lower natively) stays obfuscated (L1.4).
So a Level-3 binary is "opaque native + obfuscated bytecode fallback." The same honest threat model
holds — the process must run the code, so a host-controlling attacker can still observe it.

## Sequencing (value × independence, ascending risk)

1. **Level 1 first** — pure plumbing, no codegen, directly achieves the source-hiding goal. L1.0
   (serde sweep) is the foundation; L1.3 (bundle oracle) is the safety gate every later level reuses.
2. **Level 2** — a small delta on L1 (bootstrap + stapling). Completes "one binary, no scripts."
3. **Level 3** in order L3.0 → L3.1 → L3.2 → L3.3, then **L3.4 DCE last** (optional — L3 ships
   without it, just with larger binaries). L3.0 is the one real reuse of JIT codegen and carries its
   own no-regression gate.
4. Coverage expansion runs on its **own** track throughout, consumed by both tiers (out of scope
   here).

## Decision points (surface before committing the affected slice — do not silently pick)

- ~~**Serialization library**~~ **RESOLVED:** `postcard` (shipped in L1.0/L1.1).
- ~~**Threat model / encryption**~~ **RESOLVED (user, 2026-07-07):** obfuscation by default (L1.4,
  shipped); **keyed encryption cut on principle** — access control is the developer's *policy*, not
  the build tool's *mechanism* (see the Security model section). No crypto deps enter this arc.
- ~~**Obfuscation transform**~~ **RESOLVED:** deflate (`miniz_oxide`, pure-Rust) + fixed-seed
  scramble (shipped in L1.4). Deflate over zstd to keep the pure-Rust posture; one-line swap if
  zstd's ratio is later wanted.
- ~~**Artifact/version compat**~~ **RESOLVED:** artifacts pinned to the building runtime, mismatch
  rejected (shipped in L1.1).
- **DCE aggressiveness + reflection root policy** (L3.4): how much to strip under dynamic dispatch;
  `@reflectable` as the opt-in root set.
- **Cross-compilation** (L2/L3): host-target only in v1, or prebuilt per-target runtimes now?

## Explicitly out of scope (deferred — user-confirmed 2026-07-07)

- **JIT op-coverage expansion.** AOT functions at today's ~25% coverage via the interpreter
  fallback; expanding coverage (bitwise/`WideInt`, the rest of the collection/map/string leaves, ADT
  construction, `match` dispatch, closures/upvalues, async ops) is a **separate shared track** that
  lifts both the JIT and AOT and is sequenced independently. It raises the AOT *quality* ceiling
  (how much of an eagerly-compiled program is native vs interpreted) but is neither a prerequisite
  nor unique to AOT.
