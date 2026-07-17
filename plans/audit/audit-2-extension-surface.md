# Extension / Native-ABI Surface — Architectural Audit

Scope: `noeta-native`, `noeta-stdlib` (registration + Host), `noeta-reactive`/`noeta-reactive-abi`, `noeta-para-p2p(-net)`, `noeta-embed`, `noeta-vm` extension seams (`VmCtx`, ExtState/arena, extern types), `noeta-html`/`noeta-css`, the composed-toolchain path in `noeta-cli/src/compose.rs`. All paths relative to `/home/niklas/Code/lang/.claude/worktrees/audit-main/`.

## The actual registration & dispatch flow (as found)

```
REGISTRATION (all static data, no runtime construction)
  impl Extension (const tables: ExtModule / ExtType / ExtBundle / ExtCommand /
                  ExtTier / ExtAttribute / BodyFormatter / ExtCapability)
        │
        ├─ CLI:    run_cli → install_with_extras(std_units ∪ {html, css} ∪ shim units)
        │            └→ noeta_native::registry::install → OnceLock<Registry> DEFAULT   (process-global)
        ├─ composed toolchain: generated shim main() aggregates each package's
        │            `pub static NOETA_EXTENSIONS` and passes them to run_cli (compose.rs)
        ├─ embed (per process):  install_extensions() → same global DEFAULT
        ├─ embed (per session, IR5): Builder::with_extensions → assemble_with_extras
        │            → Box::leak(Registry) → threaded: check_all_with_registry
        │            → compile_with_sites_session_with_registry → VmSession(state.registry)
        │            → Vm.registry → isolate workers inherit (isolate.rs:569-583)
        └─ lazy fallback: ANY facade lookup → ensure() → std-only default
  SIDE-CHANNEL GLOBALS (outside the Registry story):
        noeta-reactive-abi::FOREIGN_VIEW_EXTRACTORS (RwLock<Vec<fn>>)
        noeta-native::map_key::packed_names (Mutex<HashMap>)

DISPATCH (VM; noeta-eval mirrors the same shape)
  Op::CallMethod
   ├─ receiver is NativeModule("std.json" — ROOT-QUALIFIED string)
   │    call_native_module (methods.rs:378)
   │      1. reg.find_function        → shallow/deep marshal → ExtModule.dispatch(func, &mut dyn Host,
   │         &[NativeValue]) → NativeOut → materialize_ext   (Spawn → executor.spawn_ext)
   │      2. reg.find_ctx_function    → call_ctx_function (native_ctx.rs:861)
   │           → static_dispatch_ctx(module …)  [matches bare "cell"/"reactive" — see F3: likely dead]
   │           → else reg.dispatch_ctx → CtxDispatch(func, &mut VmCtx, &[Slot])
   ├─ receiver is Extern
   │    per-site route cache keyed on SHORT type_name ptr (lib.rs:4666-4714)
   │      resolve_extern_route(reg.find_type(SHORT name))   (methods.rs:35)
   │        FastRead (arena_getter, gate open) → inline arena load, no dispatch
   │        Ctx  → call_ctx_type_method → static_dispatch_ctx_method(SHORT name: Cell/Signal/…)
   │               → else reg.dispatch_ctx_method (dyn table)
   │        Plain → call_extern_method → ExtType.dispatch(&mut dyn ExternValue, method, host, args)
   └─ bundle method (compiler-baked (module,bundle)) → reg.dispatch_bundle_method

  CHECKER: SigType → Type mapped ONCE (noeta-check/src/stdlib.rs:45-97);
           classify_use is the single use-resolution source (noeta-native/registry.rs:1112).
```

---

## Finding 1 — Extern-type runtime identity is the *short* name; the documented qualified-identity model is not enforced anywhere at runtime

**Severity: high**

**Evidence:**
- `crates/noeta-native/src/registry.rs:487-492` (doc on `ExtType::name`): "The type's *identity* (for lookup, equality, dispatch, `is`/`as`) is the **qualified** name … two types with the same short name under distinct namespaces are distinct identities."
- `crates/noeta-native/src/extern_value.rs:28-31`: `ExternValue::type_name()` "must equal the `ExtType::name`" — the **short** name. A value never carries its namespace.
- `crates/noeta-native/src/registry.rs:1243-1248`: `find_type` scans all units and returns the **first** short-name match; `resolve_type` (1259) falls back to it.
- Runtime dispatch keys on the short name everywhere: `crates/noeta-vm/src/lib.rs:4668` (`v.with_extern(|e| e.type_name())` feeds the route cache), `crates/noeta-vm/src/methods.rs:40` (`reg.find_type(type_name)`), `crates/noeta-vm/src/methods.rs:451-452` (`call_extern_method`), and `narrow_matches` derives the qualified name *via* the short-name lookup: `crates/noeta-vm/src/lib.rs:2185-2191`.
- `validate()` (`registry.rs:1369-1418`) checks unit names, qualified **module** identities, and bundle names — it never checks extern-type names, short *or* qualified.
- Std's short names are maximally collision-prone: `Counter`, `Histogram`, `Gauge`, `Signal`, `Span`, `Socket`, `Duration`, `Instant` (`crates/noeta-stdlib/src/{metrics,reactive,tracing,serve,datetime}.rs`).

**Why it matters:** If a third-party extension registers an `ExtType { name: "Counter", namespace: "acme.metrics", … }` — which the checker-side design explicitly promises to support — every runtime path (method dispatch, arena fast-read routing, `is`/`.as<T>()`) resolves `"Counter"` to whichever unit registered first (std, since `std_units()` lead every assembly). Acme's values dispatch into `std.metrics`' dispatch, whose downcast fails → confusing runtime errors; `x is acme.metrics.Counter` computes the value's qualified name as `"std.metrics.Counter"` and answers `false`. Nothing panics at install; the failure is silent and value-dependent. The static checker and the runtime disagree about what a type *is*.

**Proposed remedy (incremental):**
1. Add extern-type uniqueness to `validate()` — reject duplicate *short* names across units for now (turns silent mis-dispatch into a startup panic; zero-cost).
2. Longer term, make the value carry its identity: change `ExternValue::type_name()` to return the qualified name (or add `qualified_name()` with a default of `type_name()` and migrate), key `find_type`/route caches/`narrow_matches` on it. The per-site pointer cache keying (`lib.rs:4670`) already uses an interned `&'static str`, so a qualified `&'static str` is a drop-in.

**Perf-regression risk:** none for step 1; low for step 2 (identity strings are compared by pointer in the hot cache; only cache-miss resolution changes).

**Disposition (branch `extern-identity`):** ✅ FIXED end to end. Step 1 (assembly-time short-name refusal) landed as the stopgap; step 2 then made the value carry its identity: `ExternValue::type_name` became `type_identity` returning the pre-joined qualified `namespace.name` literal, and every runtime identity consumer — the VM route cache (pointer-keyed on the interned identity), `Registry::dispatch_method`/`resolve_extern_route` (now `find_type_qualified`), `is`/`.as<T>()` narrowing (a direct string compare, no registry walk), the read gates, the compiled-in ctx fast routes, extern map keys, and reflection (`TypeRepr::Named(identity)`, also fixing a latent VM-vs-eval divergence) — keys on it in both backends. Display stays the short name (`type_display_name`). The short-name refusal was then replaced: distinct namespaces may share a short name (proven end to end by `crates/noeta-conformance/tests/extern_identity.rs` on both backends); a duplicate qualified identity still refuses to assemble. `.noeb` needed no format bump: the checker already emitted qualified narrowing targets, and the cache key folds in the running binary's build identity.

---

## Finding 2 — Per-session registry threading (IR5) has a hole: `Session::hot_swap` checks against the process-global default

**Severity: high**

**Evidence:** `crates/noeta-embed/src/lib.rs` — `Builder::load` threads the private registry through checking and compiling (lines 352-370: `check_all_with_registry`, `compile_with_sites_session_with_registry`), and `VmSession::adopted_with_registry` stores it (`noeta-vm/src/session.rs:303-322`). But `Session::hot_swap` (embed lib.rs:466-471) runs:
```rust
let checked = noeta_check::check_all(&new_program);   // ← no registry
```
The `Session` struct (lib.rs:400-407) doesn't even retain the registry to pass — it holds only `session/source/stdout`.

**Why it matters:** The advertised model is "a session with its own assembled registry resolves native names — from type-check through runtime dispatch — against *its* extensions" (embed lib.rs:55-58). Hot-swapping an edit that uses a session-private native module or type checks against the std-only (or differently-composed) default: the swap is wrongly rejected with unknown-name errors — or, worse, if the default registry happens to contain a *different* extension with the same module name, the edit checks against the wrong signatures and swaps in code the session's VM then dispatches differently. This is exactly the "half-threaded" seam the audit brief asked about, and it sits on the crate's canonical use case (game-engine hot reload).

**Proposed remedy:** Store the `Option<&'static Registry>` on `Session` (it's already `'static` and `Copy`) and call `check_all_with_registry` in `hot_swap`. ~10 lines.

**Perf-regression risk:** none.

---

## Finding 3 — The compiled-in "fast routes" are keyed inconsistently: the module route is (very likely) dead code, and the type route bypasses the instance registry by short name

**Severity: medium (perf rot + a design wart that amplifies Finding 1)**

**Evidence:**
- `crates/noeta-stdlib/src/registry.rs:3963-3976`: `static_dispatch_ctx` matches `module` against bare `"cell"` / `"reactive"`.
- But module identities are **root-qualified** end-to-end: the compiler emits `Const::NativeModule(qualified_module(path, name))` → `"std.cell"` (`crates/noeta-compiler/src/lib.rs:1960-1966`, `97-99`), and `call_ctx_function` passes that string straight through (`noeta-vm/src/native_ctx.rs:871-876`). `"std.cell" != "cell"` → the fast route returns `None` and everything falls to the dyn table. No test asserts the route fires.
- The type-method twin (`registry.rs:3981-4008`) matches short type names (`"Cell"`, `"Signal"`, …) and is consulted **before** the VM's instance registry (`native_ctx.rs:704-718`, with the comment "the fast path stays on the global — std is in every assembled registry").

**Why it matters:** (a) The H5 perf work's module-level monomorphized route silently rotted when identities became qualified (namespaced-types arc) — the intended inlining of `signal(…)`/`cell(…)` constructor calls no longer happens, and nothing notices because the dyn path is behaviorally identical. (b) The type route hard-binds the names `Cell/Signal/Computed/Effect/View` to std's dispatch for *every* registry in the process — the mechanical half of Finding 1's hijack: even a registry-aware `find_type` fix would be bypassed by this match.

**Proposed remedy:** Match on `registry::module_name(module)` (the root-stripping helper already exists, `registry.rs:1435-1437`) *after* confirming the root is `std` — or simply key both fast routes on the qualified identity. Add a debug counter/test asserting the fast route fires for `signal.get()` and `use std.cell` programs.

**Perf-regression risk:** none — the change *restores* a fast path; the extra `split_once` on a cold-ish path is trivial. (Benchmark `signal.get/set` loops per the perf-sweep discipline anyway, since this path was explicitly measured in H5.)

---

## Finding 4 — Contracts extension authors must uphold that the API cannot enforce; several misuses compile clean and corrupt or abort the runtime

**Severity: medium**

**Evidence & inventory:**
1. **`key_capable` promises** (`noeta-native/src/registry.rs:500-503`): "no mutating methods, `cmp_value` is a total order, `hash_value` stable and content-derived." Nothing checks any of it. A non-total `cmp_value` breaks the eval backend's `BTreeMap` invariants and set canonicalization → silent wrong answers and differential divergence, not an error.
2. **`arena_getter` equivalence** (`registry.rs:508-519`): the inlined read "must behave identically to the fast path whenever the gate is open." Purely semantic; a wrong `project` fn or a forgotten `set_read_gate` window returns stale values with no diagnostic. (The comment explicitly says "the declaration is semantic, not an optimization hint" — documented, but still unverifiable.)
3. **`ExternValue::type_name` must equal `ExtType::name`** (`extern_value.rs:28-31`): a typo yields "`X` is not a registered type" at first method call — at runtime, per value, not at registration.
4. **`ExtState` borrow discipline** (`ctx.rs:41-46`): "drop the borrow before every `ctx.call`/`poll`". Violation = `RefCell` double-borrow panic mid-flush. The reactive engine itself is the pattern's proof, but a third-party author gets a VM-killing panic for an entirely natural-looking nested call.
5. **`SpawnBox::clone` panics** (`registry.rs:124-128`): `NativeOut` derives `Clone`; any dispatch that clones a `NativeOut` holding a `Spawn` (normal-looking Rust) hits `unreachable!`.
6. **`ParsedArgs` accessors `expect(…)`** (`command.rs:53-77`): a command body asking for an undeclared arg name panics the CLI rather than erroring.

**Why it matters:** The slot/retained machinery is exemplary (misuse → typed `CtxError`, never UB — `native_ctx.rs:57-62, 81-86`), but the list above is the residue where "compiles fine, corrupts/aborts at runtime" still holds. For a surface that third parties are explicitly invited onto (Phase 3), each of these is a support incident waiting.

**Proposed remedy:** (a) `debug_assertions`-gated verification: on materializing a `NativeOut::Extern`, assert the value's `type_name` resolves to a registered type; for `key_capable` types, a one-shot property spot-check (reflexivity/antisymmetry over a few clones) at install. (b) Make `SpawnBox` a separate non-`Clone` variant carrier (`NativeOut` → split a `NativeWork` out of the value enum, or return `Result<CtxOut | Work>` from dispatch). (c) Turn `ParsedArgs` accessors into `Option`-returning with `#[track_caller]` panicking wrappers.

**Perf-regression risk:** none (debug-gated / type-level).

---

## Finding 5 — `ExtType::DEFAULTS` silently defaults `namespace: "std"`; a third-party type that forgets `namespace:` squats std until publish time

**Severity: medium**

**Evidence:** `crates/noeta-native/src/registry.rs:540-542` — `DEFAULTS` sets `namespace: "std"`. The only guard is the *publish-time* lint in the composed doc build (`crates/noeta-cli/src/compose.rs:162-164`: `--lint` "refuses if the package registers any module or extern type outside its own namespace … a missing `namespace:`").

**Why it matters:** During development and for path/git dependencies that never publish, a forgotten `namespace:` makes the type `std.X` — it wins `use std.X` resolution, participates in the std short-name space (compounding Finding 1), and contradicts the namespace-protection arc's premise that `std` is reserved. The failure mode is invisible until a publish attempt.

**Proposed remedy:** Remove the default (make `namespace` mandatory — it's a compile error with a clear message at every literal), or validate at `Registry::new` that a non-`"std."`-named unit registers no `std`-namespaced types (the same check the publish lint does, moved to assembly where every path hits it).

**Perf-regression risk:** none.

---

## Finding 6 — The global default registry's lifecycle is panic-coupled to lookup order, and `Registry::new`'s panic surfaces through the embed library API

**Severity: medium**

**Evidence:**
- `crates/noeta-native/src/registry.rs:1351-1359`: `install` **panics** if anything (including the lazy std default from *any* facade lookup, `noeta-stdlib/src/registry.rs:444-446`) got there first. `install_with_extras`'s comment (`stdlib/registry.rs:459-462`) acknowledges the race: "installs eagerly so a later facade lookup cannot race in an std-only default first."
- ~60 facade call sites remain untreaded by design (`noeta-native/src/registry.rs:1420-1423`): "These stay so the ~60 call sites across the checker, backends, LSP, and CLI are untouched until each is threaded."
- `Registry::new` panics on duplicates (`registry.rs:912-915`), and `Builder::load` calls it (`noeta-embed/src/lib.rs:348-350`) — so an embedding host passing a unit that collides with std gets a **panic**, not the `Result<Session, Error>` the API promises (documented at `with_extensions`, lib.rs:328-329, but still a panic from a fallible library entry point).
- Tooling state (mandate item 3, verified): LSP/MCP/IDE and the salsa/CLI compile path are pinned to the global default — `noeta-ide/src/completion.rs:146,165`, `noeta-ide/src/lib.rs:624`, `noeta-mcp/src/stdlib.rs:115-157`, `noeta-compiler/src/lib.rs:181-182` ("The CLI/salsa/differential compile path is single-registry — the process-global default"). This is a *documented* decision and is coherent for the composed-toolchain model (the shim installs the app's extras globally, so the LSP sees them); the genuine crack is Finding 2, not the tooling.

**Why it matters:** Any library consumer (tests, a second embedding crate, a future plugin host) that performs one innocent lookup before the host's `install` turns a configuration issue into a process abort whose message points at neither culprit. Panics as an API contract are fine for the CLI binary; they are hostile in `noeta-embed`.

**Proposed remedy:** Give `Registry::new`/`install` fallible twins (`try_new → Result<Registry, AssemblyError>`), use them in `Builder::load` (map to `Error::Check`-style variant) and `install_with_extras`. Keep panic in the CLI entry. Consider making `install` after a *lazy* default a replace-with-superset rather than a panic (the lazy default is always a strict subset).

**Perf-regression risk:** none.

---

## Finding 7 — A custom `Host` is all-or-nothing: 12 supertraits, ~70 required methods, no delegating base

**Severity: medium**

**Evidence:** `crates/noeta-native/src/host.rs:545-575` — `Host` is the union of `FileSystem + Rng + Clock + Env + Os + Entropy + Ids + Network + P2pProvider + Tracing + Metrics + Logging` via a blanket impl. `FileSystem` alone requires 12 methods, `Os` 16, `Network` 8 required (websockets/durables have defaults). The embed docs advertise "or a custom `Host` implementation" (`noeta-embed/src/lib.rs:53`) — the canonical game-engine consumer — but there is no `SandboxHost`-wrapping delegation helper, `NullHost`, or macro; the doc's mitigation ("a consumer that needs only one depends on that trait instead", host.rs:535-537) helps *capability consumers*, not host *implementers*, who must satisfy the whole union to construct a backend (`Box<dyn Host>`).

**Why it matters:** The realistic path for an engine that wants "expose my world as the fs/env" is to fork or wrap `SandboxHost` by hand — ~70 forwarding methods of boilerplate that will silently miss every new capability added to the union (each new Host capability is a breaking change for every out-of-tree host).

**Proposed remedy:** Ship a delegating adapter in the ABI crate: `struct HostOverlay<B: Host> { base: B }` with all methods forwarded (or a `#[delegate]`-style macro), so a custom host overrides only what it changes. This also converts "new capability added" from *breaking every host* to *inherited default*.

**Perf-regression risk:** none (IO paths, one extra static call that inlines).

---

## Finding 8 — Two generations of marshalling vocabulary coexist, and the "lean ABI contract" crate carries a thousand lines of Ring-1 stdlib semantics

**Severity: medium**

**Evidence:**
- Legacy seam: `Arg`/`Output`/`Dispatch` + `string_method` (`noeta-native/src/lib.rs:54-128, 160-316`) — the Ring-1 string surface, value-marshalled through its *own* enums with its own `want_arity`/`want_str` helpers.
- Current seam: `NativeValue`/`NativeOut`/`Scalar` (`registry.rs:22-116`) with *its* `want_arity` twins in noeta-stdlib, plus the ctx seam (`Slot`/`CtxOut`), plus `NumScalar` (lib.rs:867-871), `TypeRecipe`, `PackedView`/`PackedField` vs `ConstraintField` vs `noeta_object::PackedKind` (three spellings of "Int|Float|F32|Bool"), `MapKey`/`PackedKeyField`, embed's `Value`, the VM's `Wire`.
- The ABI crate's own header (`lib.rs:1-8`) declares it "the contract a crate implements … plus the dep-free primitives both backends and the front-end share" — the merge is documented.

**Why it matters:** This is the core "is there ONE way?" question. The answer today: there are **two** module-function marshalling generations (Arg/Output for strings; NativeValue/NativeOut for everything registered) and **one** higher-order seam — plus ~6 auxiliary vocabularies. Each individual seam is well-justified by the backend-neutrality argument (and says so), but the *aggregate* is what a third-party author opens `noeta-native` and sees: the first 1,500 lines of "the extension ABI" are `pad_start` semantics and popcount tables. The signal-to-contract ratio actively obscures the real authoring surface (`Extension`/`ExtModule`/`ExtType`/`NativeCtx`).

**Proposed remedy:** Mechanical split, no semantic change: move the Ring-1 bodies (`string_method`, `int_method*`, `num_convert`, `format_float`, `ListMethod`/`SetMethod`/`MapMethod`) into a `noeta-native::ring1` module (or a sibling `noeta-core-sems` crate re-exported by both backends), leaving `lib.rs` as contract + re-exports. Fold `Arg`/`Output` into `NativeValue`/`NativeOut` as a later, behavior-identical step (both are supersets). Consolidate the three primitive-kind enums behind one `ScalarKind`.

**Perf-regression risk:** none for the move; low for the `Arg`→`NativeValue` fold (string methods are dispatched via enums per backend; keep the zero-alloc `&str` projection by adding a borrowed-str variant or keeping `Arg` as a `#[doc(hidden)]` internal).

---

## Finding 9 — `NativeCtx` is still a 39-method wide trait; the god-trait split stopped halfway

**Severity: medium (challenging a partially-documented decision)**

**Evidence:** `crates/noeta-native/src/ctx.rs:134-379`: one trait carrying five distinguishable concerns — slot/value marshalling (view/intern/free/call/…, 13 methods), async orchestration (spawn_io/poll/drive/cancel/advance_*/wake_*, 9), Class-3 state+arena (state/retain/…/call_thunk_into, 9), the raw-buffer packed ABI (with_packed*/make_packed_like/object_scalars_at/…, 5), and the capability broker (1) — plus three accessor-vended sub-traits (`TaskContext`, `FutureTracing`, `HotReload`) that *were* split out, with a comment (ctx.rs:361-369) explaining the sub-trait pattern exists precisely because "they are the concerns that used to grow `NativeCtx` one method at a time."

**Why it matters:** The codebase already discovered the right pattern (accessor → narrow sub-trait; capability broker for cross-extension services) and applied it only to the three newest concerns. Every backend (`VmCtx` 922 lines, `EvalCtx`) must implement all 39 methods even for concerns its extensions never touch; every *reader* of a dispatch signature (`&mut dyn NativeCtx`) gets no signal about what the dispatch can actually do (a pure `cell.get` and a full HTTP serve loop take the same capability). The packed raw-buffer group in particular is a self-contained ABI with exactly one consumer family (kernels) and would move cleanly.

**Proposed remedy:** Continue the established pattern incrementally: `fn packed(&mut self) -> &mut dyn PackedBuffers` and `fn arena(&mut self) -> &mut dyn RetainedArena` accessors (backends return `self`, exactly like `task_context()` — zero cost, per the codebase's own comment). Don't split the slot/call core; that *is* NativeCtx.

**Perf-regression risk:** none (documented in-tree: "no lookup, no allocation, one virtual indirection on already-cold paths", ctx.rs:364-365).

---

## Finding 10 — Versioning: there is no ABI version anywhere; stability rests entirely on source-level cargo unification, and the silent-break vectors are unlisted

**Severity: medium-low**

**Evidence:**
- `crates/noeta-native/Cargo.toml`: `version.workspace = true` — no ABI version constant, no stability marker in the crate.
- The composed toolchain unifies types by construction: in-workspace via `[patch]` redirecting every git dep on the repo to local paths (`compose.rs:771-819` — explicitly noting that *without* it "a `dyn Extension` from the package would not match the shim's `noeta_native::Extension` type"), out-of-workspace by pinning the running binary's tag (`compose.rs:359-371`).
- The capability broker's soundness note (`ctx.rs:471-474`): "`TypeId` is consistent within one linked program (the composed toolchain builds everything under one lockfile)".
- Dynamic loading is anticipated but absent (`stdlib/registry.rs:3959-3960`: "a future dynamically-loaded extension").
- Additive evolution is handled (`ExtFn::DEFAULTS`, `ExtModule::DEFAULTS`, `ExtCommand::DEFAULTS` — N3.6).

**Why it matters:** The design is actually sound *today*: because extensions are always recompiled against the exact toolchain source, "ABI breaks" are compile errors, not silent runtime skew — a deliberately boring answer, and the `[patch]` mechanism shows real care. What breaks **silently** is anything semantic rather than structural: marshalling projections (what `to_native_deep` produces for a new value kind), dispatch precedence (plain table before ctx table, `methods.rs:418-421`), the `arena_getter` gate protocol, `MapKey` ordering, and the `NOETA_EXTENSIONS` symbol convention (a slice of the right type but wrong content — e.g. duplicate roots — fails only at install-panic time). And the day a dynamically-loaded extension lands, the `TypeId` broker and every `#[derive]`-free struct layout become UB vectors with no version handshake to refuse the mismatch.

**Proposed remedy:** Cheap now: add `pub const ABI_VERSION: u32` (or the crate's own semver via `env!`) to `noeta-native` and have `install`/shim generation record it, so the *future* dyn-loading path has something to check; write the semantic-contract list above into `docs/Native-Extensions.md` as "what we may change under a minor bump." Adopt a policy that `noeta-native` gets a semver-meaningful version distinct from the workspace lockstep before any registry publishes native packages.

**Perf-regression risk:** none.

---

## Finding 11 — Every dispatch body re-implements, by hand, the validation its declared signature already states

**Severity: low**

**Evidence:** `crates/noeta-stdlib/src/registry.rs:3334-3361` (`uuid_method_dispatch`) is representative: the `ExtFn` tables declare `params: &[…]`/arity, then the dispatch re-does `want_arity(method, args, 0)?` and manual downcasts per arm — for ~4,400 lines in the std registry alone, and every third-party module copies the pattern (`noeta-para-p2p/src/*.rs` does the same). The checker already gates arity/types statically (the `want_*` layer is self-describedly "defensive", lib.rs:380-381), so this is triple-maintained truth: signature table, checker mapping, hand extraction.

**Why it matters:** Boilerplate volume is the top authoring-friction item on this surface, and dual maintenance drifts (a param added to the table but not the extractor compiles fine and errors at runtime).

**Proposed remedy:** A declarative dispatch macro in `noeta-native` that generates the match + extraction from the signature: e.g. `ext_fns! { fn contains(recv: str, needle: str) -> bool { recv.contains(needle) } }` emitting both the `ExtFn` table and the dispatch fn — or, less invasively, typed extractor combinators (`args.str(0)?`, `args.opt_int(1)?` on a wrapper that knows the method name). Pilot it on one new module rather than migrating std wholesale.

**Perf-regression risk:** none if the macro expands to the same match; keep the generated code inspectable.

---

## Finding 12 — Side-channel process globals survive outside the Registry story

**Severity: low**

**Evidence:**
- `crates/noeta-reactive-abi/src/lib.rs:71-93`: `FOREIGN_VIEW_EXTRACTORS`, a process-global `RwLock<Vec<fn>>` registered from dispatch bodies at first use (`noeta-para-p2p/src/synced.rs:69-78`, guarded by a `static Once`).
- `crates/noeta-native/src/map_key.rs:49-73`: `packed_names`, a process-global `Mutex<HashMap>` of packed-key field names, "process-global like the shape interner", first-registration-wins (`or_insert_with`).

**Why it matters:** Both are additive, type-directed, and thread-safe, so they're benign in the single-registry world — but they are invisible to the IR1-IR5 per-session model: two sessions with different `@packed` types sharing a short name get first-wins display (stale after a hot-swap rename); the extractor list can't be scoped or torn down per session. More architecturally: the view-extractor registry is a *second, ad-hoc* cross-extension mechanism living beside the capability broker that was built to replace exactly this shape ("it generalizes the hardcoded cross-extension seams … into one mechanism", registry.rs:783-787). `para.synced` uses **both** in the same file.

**Proposed remedy:** Model the extractor as a capability: the reactive engine asks `capability::<dyn ViewSourceProvider>` (or the extension registers the extractor on its `ExtCapability` state at init) — deleting the global. `packed_names` can move onto the Registry/session when a per-session need actually materializes; note it in the instance-registry ledger so it isn't forgotten.

**Perf-regression risk:** low — extractor resolution is on `view.expose` (cold); keep the read path lock-free if moved.

---

## Finding 13 — `validate()` gaps and silent first-wins policies at assembly

**Severity: low**

**Evidence:** `crates/noeta-native/src/registry.rs:1369-1418` validates unit names, qualified module identities, bundle/bundle-method names — but not: extern-type identities (see Finding 1), tier names across extensions, attribute names, body-formatter languages, or command names ("Must not collide with a core command", `command.rs:137` — unchecked here). `find_capability` (registry.rs:926-933) documents duplicate providers as "a configuration error the first one silently shadows."

**Why it matters:** The registry's own philosophy is "a mis-assembled binary must not start" (registry.rs:912-913). The unvalidated axes fail later and quieter: a duplicate tier name resolves first-wins in `find_ext_tier`, a duplicate `("css", …)` body formatter silently shadows, a shadowed capability provider yields the wrong engine. All are one loop each in `validate()`.

**Proposed remedy:** Extend `validate()` to cover types (qualified + short), tiers, attributes, formatter languages, capability ids, and command names; panic with the owning unit names on both sides.

**Perf-regression risk:** none (startup-only, O(n²) over dozens).

---

## Finding 14 — Per-session registries are `Box::leak`ed; the canonical embed consumer leaks one per session load

**Severity: low (challenging a documented decision)**

**Evidence:** `crates/noeta-embed/src/lib.rs:342-351`: "assembles a private registry (**leaked** to `'static`, matching the `'static` extension-data model the whole pipeline assumes)". The `'static` assumption is load-bearing everywhere (`Vm.registry: Option<&'static Registry>`, vm lib.rs:1338; every lookup returns `&'static`).

**Why it matters:** The documented rationale is real — the entire type system hands out `&'static` refs — but the crate's canonical consumer is "a game engine's scripting layer" (lib.rs:3) that may create/drop sessions for the process's lifetime; each `load` with extensions leaks a `Registry` (a small `Vec` — bounded per leak, unbounded in count). The units themselves being `'static` is fine (they're statics); it's the per-load *assembly* that leaks. Worth either interning (one leaked registry per distinct unit-set, memoized by unit pointer set — hosts overwhelmingly reuse one set) or an explicit doc note that hosts should build the `Builder` extension set once and reuse it.

**Proposed remedy:** Memoize `assemble_with_extras` results keyed on the (sorted) unit pointer list in a process `OnceLock<Mutex<HashMap<…>>>`; identical sets share one leaked registry. ~20 lines, keeps `'static` intact.

**Perf-regression risk:** none.

---

## What's already good

- **The slot-table ownership model** (`VmCtx`) genuinely solves the leak class it was built for: seeds borrowed, table-owned refs released on `Drop`, freed-slot misuse is a typed error, never UB (`noeta-vm/src/native_ctx.rs:1-8, 151-165`). The pooled tables and in-place future-slot reuse show real hot-path care.
- **One shared dispatch body per module/type, run by both backends** — the differential-by-construction promise is structurally enforced, and the exhaustive-enum trick (`ListMethod` etc.) makes cross-backend drift a compile error.
- **The value/heap boundary does not leak**: `NativeValue`/`NativeOut`/`Slot`/`Retained` keep NaN-boxing, shapes, and GC entirely backend-side; extern boxes hold plain `Send` data and arena *ids*, so the leak oracle and cycle collector see everything (`ctx.rs:33-39` explains why — and it's the right reason).
- **The capability broker** (`ExtCapability` + `capability::<dyn T>`) is a clean, sound (safe-downcast) generalization, and `noeta-reactive-abi` is a model contract crate — trait in its own crate, neither side names the other.
- **The composed-toolchain `[patch]` mechanism** (compose.rs:771-819) shows the type-unification problem was understood and solved deliberately, with the failure mode written down.
- **`classify_use` / `SigType→Type` each live exactly once** — the resolution and signature stories have single sources of truth with tests pinning the render conventions.
- **`MapKey`** is a small gem: snapshot semantics keeping keys out of the GC, content-only hashing preserving the zero-alloc `&str` probe, one `Ord` shared by both backends' containers.
- **Intent documentation** throughout is exceptional — nearly every trade-off this audit checked had its rationale within 20 lines of the code, which made distinguishing "deliberate" from "rot" (Findings 3, 5) actually possible.
