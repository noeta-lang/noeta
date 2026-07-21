# Native Extensions

Native modules (like `math`, `json`, `fs`) are not hardcoded into the runtime — they are registered through one uniform seam, and the core `std` modules are the *dogfooded* extension registered *through* that seam rather than special-cased.

> [!NOTE]
> Since package-manager Phase 3, this seam is **open to third-party packages**: a dependency can ship a Rust crate that registers native modules, types, and CLI commands, statically composed into the consumer's toolchain by cargo. See [Writing a native package](#writing-a-native-package) below.

## Why a registry

Hardcoding native modules created four parallel seams that could drift: a `NativeModule` enum, per-backend `call_vec`/`call_json`/… dispatch, and checker tables of known modules. The registry dismantles all four into one mechanism — and it makes differential agreement *structural*: one shared dispatch function per module, not two mirrored copies. The design test the work held itself to: *could `vec`/`quat` be deleted from core and re-added as a third-party crate with no API change?*

## Two crates: `noeta-ext-abi` (the ABI) and `noeta-stdlib` (the batteries)

The contract a native extension implements lives in its own lean crate, **`noeta-ext-abi`**: the
[registry] vocabulary (`Extension`/`ExtModule`/`ExtType`/`ExtFn`/`SigType`/`RetTy`/`TypeArgWrap`/`TypeRecipe`,
`NativeValue`/`NativeOut`/`Scalar`), the `Host` capability seam and its traits, the `ExternValue`
contract (`ExternBox`), `MapKey`, the async `ExternIo`/`Executor` seam, and the dep-free Ring 1
primitives. Its only dependencies are `compact_str`/`equivalent`/`hashbrown` — none of core's
batteries (no crypto, uuid, JSON, or HTTP client). **`noeta-stdlib`** depends on it, re-exports it
(`pub use noeta_ext_abi::*` — the `core`/`std` relationship), and layers the concrete `std` modules
and their heavy deps on top. So a third-party extension (and internal mid-end crates like
`noeta-ir`) links the lean ABI, not the whole standard library. Since package-manager Phase 3 this
is the *consumed* boundary too: an out-of-tree entry crate depends on `noeta-ext-abi` alone, and the
crates are versioned + git-tagged for the composed shim to pin (see
[Writing a native package](#writing-a-native-package)).

## The seam

The registry vocabulary (`noeta-ext-abi`, `registry.rs`; the concrete `std` registration and dispatch
router are in `noeta-stdlib`) is built on a **neutral value-marshalling** layer:

- `NativeValue` — the argument view: `Scalar`, `Str`, `Bytes`, `Object { fields }`, `List`, and so on.
- `NativeOut` — the result view, including the bulk `Scalars(ScalarVec)` form (one typed vector for a whole reduction result — the `Bytes` idea applied to primitive lists).

Two per-backend functions, written once each — `marshal_native_arg(&Value) -> NativeValue` and `materialize_native(NativeOut, …) -> Value` — replace all the duplicated dispatch. A module function is then just a `DispatchFn = fn(&mut dyn Host, &[NativeValue]) -> Result<NativeOut, StdError>`, **shared across both backends** so the differential holds by construction. The `Host` capability (see below) is threaded through so `fs`/`time`/`random`/`env`/`args` migrate too (pure modules ignore it).

Registration is declarative:

```rust
trait Extension {
    name;
    root() -> &str;              // namespace root; defaults to name(). Module identity is
                                 // <root>.<module.name> (`std.http.client`), so two extensions
                                 // with distinct roots never collide (`std.http` vs `guzzle.http`).
    modules() -> &[ExtModule];   // each with ExtFn { name, params, ret }, one dispatch, and an
                                 // optional `ring` (the native-dep Cargo feature it lives behind)
    types()   -> &[ExtType];     // first-class value types (see below)
}
```

**The registry is an assembled list of extension units.** Core's `std` is not one monolith but
several in-tree `Extension` units sharing the `"std"` root — `CoreExtension` (always-on) plus one per
capability with a separable identity (`HttpExtension`, `CryptoExtension`, `IdExtension`, the
`vec`/`quat` `VecExtension`). Every lookup (`find_module`/`find_type`/`commands`)
iterates the whole registry filtered by root, so the split is invisible to resolution — it's the
**dogfood of the multi-extension registry a package plugs into**: a third-party package registers as
another unit under its own root. The `para` namespace is the first first-party capability to take that
exit for real — the p2p/local-first stack (`ParaP2pExtension`, root `para`, in the `noeta-para-p2p`
crate) left `std` to ship as the non-default `para-p2p` package, alongside the pure-Noeta `para.html`
liveview package. Each `ExtModule` declares its `ring: Option<&str>` — the single
source of truth for which Cargo feature gates its heavy native deps in a tailored `noeta build
--native` (`std.http.client` → `ring-http-client`, the ~3 MB reqwest/TLS tree; `None` = always-on
core). The footprint scan reads it off the registry, so there's no hand-maintained module→ring table.

`params` and `ret` use `SigType`, a small signature vocabulary (noeta-stdlib cannot see the checker's `Type`); `noeta-check` maps each `SigType` to a real `Type`, so the registry is the single source of truth that *both* the checker and both backends read. A parameter wrapped in `SigType::Optional(&…)` is **trailing-optional** (`client.get(url, headers?)`): the checker derives the required-argument count from the first `Optional`, and the dispatch reads the slot with `args.get(i)`, supplying its own default when the call omits it — so optional params cost no backend change and no default-value machinery.

## First-class types: `ExtType` and `ExternValue`

An extension contributes a **value type** the way it contributes a module. An `ExtType` declares a short display name plus a `namespace`; the type's **identity** is the qualified `namespace.name` (`std.id.Uuid`) — what the checker keys `Type::Named` on and what every value returns from `ExternValue::type_identity` (one pre-joined `&'static` literal, so dispatch compares pointers, never formats). Runtime method dispatch, `is`/`.as<T>()`, map-key capability, and reflection all key on that identity, so two extensions may register the same short name under distinct namespaces (`std.metrics.Counter` and `acme.metrics.Counter` coexist; the registry refuses only a duplicate *qualified* identity at assembly). Humans still see the short name everywhere — diagnostics and `type_of` display strip the namespace, exactly like namespaced user types. A signature that names a shared short name spells it qualified (`SigType::Named("acme.metrics.Counter")`); the checker resolves either spelling. Beyond identity an `ExtType` carries an instance-method signature table, one shared method dispatch, and a `key_capable` flag. The value behavior lives on one trait, `ExternValue` — equality, ordering, hashing, display, clone — and each backend hosts every extern type through a **single** variant (`Payload::Extern` in the VM, `Value::Extern` in the tree-walker), so a new native type touches no backend code at all.

The method dispatch has ONE signature covering the whole {pure, mutable} × {host-free, effectful} matrix: the receiver arrives `&mut` (a pure method just doesn't mutate) and the `Host` is always passed (a pure method just doesn't touch it). Three core types prove the corners: `Uuid` (pure, byte-ordered, key-capable — it can key a `Map`/member a `Set`), `FileHandle` (mutable cursor, fs-effectful methods, not key-capable), and `crypto`'s `Hasher` (mutable but host-free — `update` mutates the receiver through the shared cell without ever touching the `Host`). Effects reach the world only through the `Host`; construction of effectful values stays in module functions (`fs.open`).

## Async functions: the `ExternIo` seam

An extension implements an **async** function without ever seeing the executor: its dispatch returns *work* (`NativeOut::Spawn(descriptor)`) instead of a value, and the backend tickets the descriptor on its executor and hands back a `Future`. The descriptor has two bodies — `run_sync(host)`, which the deterministic sandbox executor always runs **at spawn** (so an extension's async function is differential-deterministic no matter what its real body does), and an optional real body (a blocking closure for the runtime's blocking pool, or a native future) for true concurrency under `noeta run`. No real body means the real executor degrades to the sync body at spawn — correct, just serial. The `fs.*_async` family is the dogfood: its descriptors live in the same registry crate, and adding `exists_async`/`remove_async`/`list_async` touched no backend code.

## Higher-order functions: the `NativeCtx` seam

A plain dispatch is value-in/value-out — it cannot take a **closure argument**, call it back, poll futures, or drive the scheduler. Functions that need those register in a module's **ctx table** instead: the dispatch receives its arguments as opaque **slots** (indices into a per-call table of backend values it never sees) and re-enters the backend through one capability trait, `NativeCtx` — `call` a callable slot, `spawn_io`/`timer`/`poll`/`drive` futures, `advance_tasks`/`advance_clock` the scheduler, plus list access and argument probes. Each backend implements the trait once; the slot table owns the refcount discipline centrally (retain on insert, release on free/drop, arguments borrowed from the caller's registers), so a dispatch structurally cannot leak. The dispatch body stays a single shared `fn`, so the differential holds by construction — extended to orchestration code that was previously mirrored per backend as hardcoded `Builtin`s.

The whole former `Builtin` family is the dogfood: `task.sleep`/`all`/`race`/`map_bounded` (drive loops over `call`/`poll`), `server.serve` (the accept→dispatch→reply loop, including the recover-from-abort pattern: a handler abort becomes a 500 and the loop continues), and all of `std.reactive`.

## Persistent state: the retained arena and `ExtState`

An extension that owns **language values across calls** (reactive's graph; a future ORM or collection type) uses two per-run capabilities. The **retained arena** holds values the extension keeps: `retain(slot) -> Retained` moves a value in, `retained_get`/`retained_set`/`release_retained` read/replace/release it. The structural rule: extension-held values **never live inside extern boxes** (an `ExternValue` is `Send`; backend values are not) — a box carries only plain `Retained` ids, and the values sit backend-side where the refcount discipline, the leak oracle, and the cycle collector see them (the arena is an enumerable root set, released destructor-aware at teardown). **`ExtState`** (`state(key, init)`) holds the extension's own Rust data — the reactive graph is one, storing only arena ids.

Generic extern types ride on the same signature vocabulary: a constructor returns `SigType::Generic("Cell", &[Var(0)])` and method signatures reference the receiver's type arguments as `Var(i)`, so `Cell<int>.set("x")` is a static E0007 with no checker special-casing. Hot accessors can be **declared**: `ExtType::arena_getter` marks a method as a gated arena read ("this method's whole behavior is: return the receiver's retained entry"), and the backend inlines it at the call site behind a route cache while the extension's **read gate** is open — which is how a migrated `signal.get()` measures *faster* than the hardcoded builtin it replaced. The extension closes the gate for exactly the windows where the full dispatch does more (dependency tracking while a body runs; a stale memo), and the tree-walker always takes the full dispatch, so the differential proves fast ≡ full on every fixture.

`std.cell` (`Cell<T>` with `get`/`set`/`update`) is the minimal Class-3 client; `std.reactive` is the full one — graph, flush loop, coalescing, and the E0045 runaway guard are all ordinary Rust in its dispatches. Neither backend knows reactivity exists.

## Cross-extension capabilities: the capability-broker seam

When one extension needs a *service another extension provides* — not the host, another extension — it goes through the **capability broker**. The motivating case: `para.synced`'s CRDT-backed signal *is* a node in the same reactive graph as core `std.reactive`, so it must reach that engine to create its node, subscribe a reader, and wake dependents. The engine lives out-of-reach in another crate's per-run `ExtState`, and `Box<dyn Any>` downcasts only to a *concrete* type — never to a trait — so without a broker the consumer would have to name the engine's private struct (or the engine expose it). The broker turns that into a **trait contract discovered by type**:

- **Contract.** The capability is an object-safe trait in its own small crate — `noeta-reactive-abi`'s `ReactiveSource` (`create_source` / `read_source` / `wake`). Both provider and consumer depend on that crate and on nothing of each other. New capabilities are new such crates; `noeta-ext-abi` never names one.
- **Provide.** The provider declares an `ExtCapability` on its `Extension::capabilities()` — the trait's `TypeId`, the `ExtState` key that backs it, and a `build` thunk that wraps the state as the trait object. `CoreExtension` declares the `ReactiveSource` provider, backed by the same `"std.reactive"` slot its own dispatches use, so reaching the engine either way is the same cell.
- **Consume.** `capability::<dyn ReactiveSource>(ctx)` returns `Some(cap)` when some installed extension provides it (`None` otherwise — an honest "is that engine even loaded?"). The handle **owns a clone of the backing `ExtState`**, so it coexists with `&mut dyn NativeCtx`: each method takes `ctx` and borrows the engine only for its own work, releasing before any re-entry (the flush runs user effects, which re-enter reactive). Recovery is unsafe-free — the provider boxes a `Box<dyn Trait>` (a sized fat pointer) erased as `Box<dyn Any>`, and the consumer downcasts back to exactly that.

The payoff is that `NativeCtx` stops accreting one method per cross-cutting concern: a new collaboration — including between an out-of-tree package and core — is a trait crate plus a declaration, no ABI edit and no side naming the other's types. `TypeId` is consistent within one linked program (the composed toolchain builds everything under one lockfile), which is what makes the by-type lookup sound.

**Sibling mechanism — backend-service sub-traits.** The broker is for one *extension's* state vended to another. The concerns that used to grow `NativeCtx` flat — the task-local tracing context, the future-completion tracing hook, the hot-reload channel — are the *scheduler's own* state exposed to extensions, not an extension's, so they take a lighter form: small `TaskContext` / `FutureTracing` / `HotReload` traits (in `noeta-ext-abi`) reached via `ctx.task_context()` / `ctx.future_tracing()` / `ctx.hot_reload()`, where the backend just returns `self`. No `ExtState`, no `TypeId` lookup, no owned handle — one virtual indirection on cold paths — because a backend service reaching its own hot scheduler fields must not be forced behind a shareable `Rc<RefCell<…>>` the broker would require. Same end (nothing new lands on the flat trait; the sub-traits can move to their own crates when `std.tracing`/`http.serve` go out-of-tree), matched to who owns the state.

## Raw buffers: `with_packed` and the bulk-kernel ABI

A `List<packed>` is stored as one contiguous byte buffer, and a bulk kernel (a SIMD-amenable column reduction, an image transform) wants exactly those bytes — with **zero per-element traffic**. Three ctx capabilities provide it (package-manager N3.4):

- `with_packed(slot, |view, bytes| …)` — borrow the element layout + raw buffer (the `with_extern` shape). The layout arrives as a neutral read-only `PackedView { fields, byte_size, column, count }`, because the backends hold different concrete schema representations — the same reason `NativeValue`/`SigType` exist.
- `with_packed_mut(slot, |view, bytes| …)` — transform the buffer **preserving value semantics**: the callback gets a uniquely-owned copy-on-write buffer (in place only under proven sole ownership), and the transformed list arrives as a fresh slot; the input value is never observably mutated.
- `make_packed_like(like, bytes)` — allocate a result list sharing an existing packed slot's element schema (schemas are backend-interned; the seam names them, never builds them).

The element-wise *fallback* (a boxed, non-packed operand) is expressible in the same shared dispatch through the fused structural reads `object_scalars_at`/`make_object_like_element` (one reused scalar buffer, no per-element slots), and a reduction returns its whole result as one typed vector — `NativeOut::Scalars(ScalarVec::F32(…))` — so the backend converts it in a single pass. The dogfood is the `vec.*_all` family: `add_all`/`sub_all`/`scale_all`/`dot_all`/`length_all` were the **last per-backend native intercepts** in either backend; they are now one registered ctx dispatch, perf-gated at or below the old special-cased numbers (`tests/bench/pm-native/`). A third-party crate registering a column kernel for the *consumer's own* `@packed` type is proven end-to-end in the composition test suite.

## Method bundles: `impl vec.Kernels for Px {}`

Raw-buffer kernels as free functions are structurally connected to the data (`vec.dot_all(xs, ys)`
accepts any 3×`f32` packed list) — invisible to the checker and the editor. A **method bundle** is
the nominal binding on top (kernel-methods arc): a module registers a named set of methods with a
**structural constraint** (`ExtBundle { name, constraint, methods, ctx_dispatch }` on
`ExtModule::bundles` — each method `Element`, on a value of the bound type, or `Bulk`, on a
`List<T>` of it), and a user type opts in explicitly:

```noe
use std.{vec}

@packed struct Px { x: f32; y: f32; z: f32 }
impl vec.Kernels for Px {}          // constraint checked HERE, at compile time

d  = xs.dot_all(ys)                 // Bulk: methods on List<Px> — same kernel as vec.dot_all
v2 = v.normalize()                  // Element: methods on Px itself
```

The binding is what makes the whole toolchain smart: the impl site validates the shape requirement
(three `f32` fields — a mismatch is a compile-time diagnostic naming expected vs found), method
calls type nominally (`SameAsArg(0)` = the receiver's own type, so `xs.add_all(ys)[0].x` resolves
statically), member completion lists the bound methods, and conflicts are rejected receiver-aware
(an `Element` method against the type's own methods/fields, a `Bulk` method against built-in list
methods). Dispatch is **call-site-resolved**: the checker bakes the `(module, bundle)` route into
the compiled call — zero runtime discovery, an empty list receiver works, and the method form
measures at parity with the module-function form (`tests/bench/kernel-methods/`). The flip side:
bundle methods are not reachable through a `dyn` receiver (`dyn` stays the escape hatch; a runtime
binding table would be additive). `std.vec`'s `Kernels` is the dogfood; a third-party bundle over
the consumer's own packed type is proven through toolchain composition in the CLI e2e.

## Writing a native package

A dependency package ships native code by naming an **entry crate** in its manifest:

```toml
# the package's noeta.toml
[package]
name = "acme/imgfx"
version = "1.0.0"
native = "native"        # relative dir containing the entry crate's Cargo.toml
```

The entry crate is an ordinary Rust library that depends on `noeta-ext-abi` and exports its extension units as a slice — one crate, any number of units (core's own `std` is six units in one crate):

```rust
use noeta_ext_abi::registry::{ExtFn, ExtModule, Extension, /* … */};

struct ImgfxExtension;
impl Extension for ImgfxExtension {
    fn name(&self) -> &'static str { "imgfx" }          // root defaults to name()
    fn modules(&self) -> &'static [ExtModule] { /* fx, … */ }
    fn types(&self) -> &'static [ExtType] { /* … */ }
    fn commands(&self) -> &'static [ExtCommand] { /* noeta fx-info, … */ }
}

/// The composition convention: the symbol the composed toolchain links.
pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&ImgfxExtension];
```

Registration literals should spell only what they use and default the rest — `ExtModule { name, functions, dispatch, ..ExtModule::DEFAULTS }` (same for `ExtType`, `ExtFn`, `ExtCommand`) — so a future optional field is additive rather than breaking.

**What composition does.** The consumer's app never configures any of this: when `noeta run`/`check`/`build`/… sees a dependency graph with native crates, it generates a ~20-line shim crate (depend on the `noeta-cli` *library* + each entry crate; `main` passes the aggregated `NOETA_EXTENSIONS` units into `run_cli`), builds it with cargo, caches the binary content-addressed (keyed on the toolchain's build identity + each entry crate's tree), and **exec-delegates**. The composed binary *is* the app's toolchain: the checker sees the extension's signatures (a wrong-typed argument to a native function is a static error), the LSP its completions, the CLI its commands. A pure-Noeta app never touches any of this. The one requirement composition adds: a consumer of a native-dep package needs a **Rust toolchain** on PATH (the diagnostic says so by name when it's missing) — the composed build then runs once per dependency-set change, and every later invocation is a single exec.

The toolchain's own source resolves in order: `NOETA_TOOLCHAIN_SRC` (a checkout override, hermetic setups) → the workspace the running binary was built in (path deps — the development norm) → a git dependency pinned to the running binary's version tag (`noeta-cli = { git = …, tag = "vX.Y.Z" }` — cargo's own git cache does the fetching).

**Versioning policy (pre-1.0).** The consumed crates (`noeta-ext-abi`, `noeta-cli` as a lib, `noeta-stdlib`) are versioned and git-tagged together (`v0.1.0` first). A composed shim pins the toolchain by that tag and cargo unifies the extension's `noeta-ext-abi` onto the same source, so compatibility is ordinary source-level semver: **pre-1.0, a minor bump may break extension code**; patch releases are additive. `#[non_exhaustive]` is deliberately not used — the `..DEFAULTS` convention is the additive-evolution mechanism (see the N3.6 audit in the package-manager arc ledger, `plans/` git history).

**Out-of-tree packages need exactly one copy of the toolchain.** A standalone package repo can't path-depend the noeta monorepo, so its entry crate names its toolchain crates by **git on the noeta repo**:

```toml
# a standalone package's native/Cargo.toml
[dependencies]
noeta-ext-abi = { git = "https://github.com/…/noeta", tag = "vX" }
```

The subtlety is a Rust one: a type's identity includes *which compiled copy of the crate* it came from, so if `noeta-ext-abi` is compiled twice — once for the shim, once for the git entry crate — its `Extension` trait exists **twice** as two unrelated types, and the shim's `units.extend_from_slice(ext0::NOETA_EXTENSIONS)` no longer type-checks (a `dyn Extension` from one copy doesn't satisfy the other). The whole graph must resolve `noeta-ext-abi` to **one** source. Two cases:

- **The consumer runs a released (git-tag) toolchain.** The package pins the same tag the toolchain does, cargo sees one git source at one revision, and unification is automatic — nothing else to do.
- **The consumer runs a workspace (local-path) toolchain** — the development norm, and while iterating on the toolchain itself. Now the shim's `noeta-ext-abi` is a *path* and the package's is *git* — two sources. The composer closes this by injecting a **`[patch]`** into the shim that rewrites every `crates/*` member of the noeta repo to the consumer's exact path, so the package's git-deps (and their transitive `workspace = true` deps) all collapse onto the one local copy. The patch key is the toolchain's own `repository`, overridable with **`NOETA_TOOLCHAIN_REPO`** for a fork, a private mirror, or a local `file://` clone (it must equal the URL the package's `Cargo.toml` declares). Cargo *does* fetch the git source before applying the patch, so the toolchain repo must be reachable — fine for a public repo.

So a first-party-but-out-of-tree package (the `para` family) uses the git-dep form; the same `[patch]` machinery serves any third-party native package that git-depends `noeta-ext-abi`. In-tree packages keep a path dep and never touch any of this.

**Where heavy dependencies belong.** An extension whose implementation needs a heavy native tree should either put the effectful part **behind the `Host` capability seam** (the runtime side, where `noeta build --native` can gate it behind a ring feature — how `std.http`'s reqwest tree stays out of non-http binaries) or accept that its whole crate is the include/exclude unit. Unconditional heavy deps in an always-linked crate cannot be dead-code-eliminated per-program; core's own `crypto`/`id` (~65 KB of sha2/bcrypt/uuid, unconditional in `noeta-stdlib`) are the recorded won't-do-with-trigger example — the extension-unit split makes gating them mechanical if a size budget ever demands it.

### Composition for a shipped artifact — the lean runner

The composed toolchain above is the *development* binary. A **shipped** artifact is composed differently: when `noeta build` sees native runtime dependencies, it composes a **lean base** carrying your extension's runtime units but **none of the toolchain** — no fmt, no LSP, no DAP, no formatter parsers. The base's form matches the emit:

- **`--exe`** composes a **runner binary** — the same aggregation of `NOETA_EXTENSIONS` units, but the base is the lean `noeta-runner` (not `noeta-cli`), and `main` calls `run_stapled_with_extensions` instead of `run_cli`; the program's bundle staples onto it.
- **`--native`** composes an **AOT-runtime staticlib** — a `staticlib` shim on `noeta-aot-runtime` (its own C `main` off, your program's stdlib rings forwarded) whose `main` installs the units via `run_embedded_with_extensions`; the `cc` link combines it with the program's AOT machine-code object. So a native-dependency app compiles to a self-contained native binary that still resolves your native modules.

Each composition carries your extension's **runtime** capabilities (modules, types, tier handlers) only. The compositions cache separately by kind; a pure-Noeta app skips composition and uses the stock lean runner / `libnoeta_aot.a`.

### Shipping dev capabilities — gate them behind a feature

An `Extension`'s capabilities split by *kind*: `modules`/`types`/`tiers`/`commands` are **runtime** (needed to run the program); `body_formatters` (the tier-body formatter `noeta fmt` uses) is **dev-only** — it and its parser (a CSS/HTML/… reformatter is a *parser*, i.e. attack surface) must never ride into a production binary. A single crate that ships both a runtime tier handler *and* its formatter is a **mixed package**; keep the formatter out of shipped artifacts by gating it — and any heavy formatting dependency — behind a Cargo feature:

```toml
# the native crate's Cargo.toml
[dependencies]
malva = { version = "…", optional = true }   # a CSS reformatter — a parser

[features]
fmt = ["dep:malva"]                            # OFF by default
```

```rust
impl Extension for MyExtension {
    fn modules(&self) -> &'static [ExtModule] { … }   // runtime — always compiled
    fn tiers(&self)   -> &'static [ExtTier]   { … }   // runtime — always compiled

    #[cfg(feature = "fmt")]                            // dev — compiled only when asked
    fn body_formatters(&self) -> &'static [BodyFormatter] { &[("mylang", reflow)] }
}
```

Because the feature is **off by default**, every shipped base (the composed runner *and* the composed AOT runtime, both built with default features) never compiles the formatter or links `malva` — the shipped artifact is lean automatically, with no per-dependency configuration by the app author. The composed **dev toolchain**, by contrast, turns this feature **on**: name it `fmt` (the conventional dev-capability feature) and the toolchain composition enables it automatically, so `noeta fmt` reflows your tier's bodies. Only a feature your crate actually declares is enabled, so the convention is opt-in — a pure-runtime crate that declares no `fmt` feature is untouched. The same shape works for any dev-only capability whose implementation drags in a parser or other heavy tree.

## Extension commands

An extension can contribute a CLI subcommand (`ExtCommand`: name, help, typed `ArgSpec`s, and a `run` fn) — the in-process `cargo clippy` model. The CLI augments its clap parser with each registered command (so `noeta --help` lists them with real parsing/validation) and dispatches a matched name to the extension, which drives a narrow `CommandCtx`: load + check + run a program file on the real host, optionally appending a synthesized trailing entry call. `noeta serve` is the proving client — it is `SERVE_COMMAND` in the std extension, whose entry call is the exact same `server.serve(port, fetch)` a program can write directly.

## The `Host` capability

All host-coupled effects — filesystem, clock, PRNG, `env`/`args`, the operating system (`os`: subprocess exec + spawn/lifecycle control + system introspection), entropy, ids, the network, and the three telemetry signals — go through one `Host` trait (eleven mandatory capability traits, blanket-impl'd), plus one **policy** seam, `P2pProvider`: a host declares through `real_p2p() -> Option<RealP2pConfig>` whether **real** peer networking is permitted here (and with what app-id) — `RealHost` returns `Some`, the deterministic hosts the default `None`. Note it hands out **no transport**: no host implements `P2p` at all (that moved to the `para.p2p` extension — see below). Two implementations exist: `SandboxHost` (deterministic in-memory VFS, logical clock, seeded RNG, a **pure network responder**, and a scripted exec command set — what the differential always runs) and `RealHost` (real disk, real env, real subprocesses, per-isolate tokio, and a real reqwest client — what `noeta run` uses, never differential-tested).

**P2p is a capability an *extension* provides, not the host — the whole transport.** When the p2p stack left `std` for the non-default `para` package, `P2p` stopped being a mandatory arm of `Host`; and the transport itself then moved out of the hosts **entirely** into the `para.p2p` extension. The extension owns one `P2pBackend` (`Arc<Mutex<dyn P2p + Send>>`) in per-run ctx state (`ExtState`), created on first use from the host's `real_p2p()` policy: the **real p2panda node** (`noeta-para-p2p-net`) when the host permits real networking *and* the extension is built with its `ring-p2p` feature, otherwise the deterministic **loopback broker** (`noeta_ext_abi::P2pBroker`, dep-free). Both implement `P2p`; the surface reaches either through one `with_p2p` seam. So **no host implements `P2p` at all** — `RealHost` included — and `noeta-host-real` links no p2panda: the entire iroh/QUIC tree travels with the package (a non-`para` `--native` binary is ~4 MB, a `para` one ~27 MB). The wrinkle the seam solves: the async `p2p.receive` leaf is `Send` while `ExtState` is not, so the backend lives behind a `Send` `Arc<Mutex<…>>` the receive descriptor captures at spawn — the ABI that lets an extension own an async-reachable host capability. This is the same "simulate deterministically, deploy real" split as the async executor and isolate scheduler. The network capability (http arc) set the async pattern: `RealHost` overrides `net_spawn` to hand the executor a genuine `RealBody::Async` reqwest future while the sandbox resolves at spawn; `os.exec_async` follows it with a `RealBody::Blocking` subprocess body.

## Call-site-typed functions: `module.func::<T>(args)`

Some native functions build a value of a type named *only at the call site* — `json.parse::<Point>(text)` — something a user genuinely cannot express in-language. Any extension can declare one; the mechanism is registry-driven, not `json`-hardcoded.

**Declaration.** A call-site-typed function lives in a **separate table** from ordinary functions — `ExtModule::typed_functions`, dispatched by `ExtModule::typed_dispatch` — because the turbofish form `f::<T>(x)` is a distinct call surface from a plain `f(x)` (the two may legitimately share a name: `json.parse` is both a dynamic `parse(text): dyn` in `functions` and a typed `parse::<T>: T` in `typed_functions`). Each entry declares `RetTy::TypeArg(wrap)`, where the `TypeArgWrap` says how the turbofish `T` is wrapped in the declared result:

- `TypeArgWrap::Plain` — the result is `T` itself (`json.parse::<T>(): T`, the aborting door).
- `TypeArgWrap::Option` — the result is `Option<T>`.
- `TypeArgWrap::Result(SigType)` — the result is `Result<T, E>` where `E` is the named error type (`json.try_parse::<T>(): Result<T, JsonError>`, the recoverable door).

The checker reads the wrap to type the call, and validates arguments against the declared `params` with the ordinary native-argument machinery — so a wrong-arity or wrong-typed argument is the same static `E0007` a plain call gets, and a turbofish on an unknown or non-call-site-typed function is a clear `E0005`.

```rust
const BUILD_TYPED_FNS: &[ExtFn] = &[ExtFn {
    name: "make_default",
    params: &[],
    ret: RetTy::TypeArg(TypeArgWrap::Plain), // make_default::<T>(): T
}];

ExtModule {
    name: "build",
    typed_functions: BUILD_TYPED_FNS,
    typed_dispatch: Some(build_typed_dispatch),
    ..ExtModule::DEFAULTS
}
```

**The recipe contract.** The grammar `module.func::<T>(args)` is an atom (`Expr::TypedModuleCall`). The checker resolves `T` into a neutral `TypeRecipe` (scalar / unit / option / list / string-keyed map / declared-order struct — *no* enum/class/unconstrained generic, which have no recipe and are a compile-time error at the call), records it at the call site, and a shared lowering bakes it into a `TypedModuleCall` IR node the VM transcribes to `Op::TypedModuleCall`. At dispatch, both backends marshal the arguments, look up the module's `typed_dispatch`, and call it threaded the `&TypeRecipe`:

```rust
fn build_typed_dispatch(func: &str, host: &mut dyn Host, args: &[NativeValue], recipe: &TypeRecipe)
    -> Result<NativeOut, StdError>
```

The dispatch returns a `NativeOut` tree **already carrying its declared wrapper** — `NativeOut::Ok`/`Err` for a `Result` shape, `NativeOut::Some`/`None` for `Option`, a plain value tree for `Plain` (a `Plain` door signals an unrecoverable failure with `Err(StdError)`, a runtime abort; a recoverable door never uses the `Err` channel, returning its `Err` arm *inside* the `NativeOut`). The backend materializes that one tree with no per-function wrapping logic — the reference interpreter through its real registered type, the VM through a fresh same-name shape (method dispatch is name-keyed) — so the two agree by construction. `json.parse::<T>`/`try_parse::<T>` are registered exactly this way; nothing about them is special-cased in the checker or either backend.

### Call-site-typed **methods**

An extern *type* gets the same surface through `ExtType::typed_methods` / `ExtType::typed_dispatch` — `resp.json::<User>()`. The rules are the module ones verbatim: a separate name space (a name may appear in both `methods` and `typed_methods`), a required `RetTy::TypeArg` return, and the same recipe contract. The dispatch just also takes the receiver:

```rust
fn response_typed_dispatch(recv: &mut dyn ExternValue, method: &str, host: &mut dyn Host,
                           args: &[NativeValue], recipe: &TypeRecipe) -> Result<NativeOut, StdError>
```

```rust
const RESPONSE_TYPED_METHODS: &[ExtFn] = &[ExtFn {
    name: "json",
    params: &[],
    ret: RetTy::TypeArg(TypeArgWrap::Result(SigType::Named("JsonError"))),
}];

ExtType {
    name: "Response",
    typed_methods: RESPONSE_TYPED_METHODS,
    typed_dispatch: Some(response_typed_dispatch),
    ..ExtType::DEFAULTS
}
```

One subtlety worth knowing if you are debugging this path: a turbofish method call reaches the checker as **either** `Expr::TypedModuleCall` (bare-identifier receiver with one type argument — `r.json::<T>()`) or `Expr::TypedMethodCall` (anything else — `get(u)?.json::<T>()`). The split is purely syntactic and predates this feature; both spellings mean the same thing and both lower to `Op::TypedMethodCall`. What distinguishes a native typed call from an ordinary **erased** generic-method instantiation is not the syntax but whether the checker found the name in the receiver type's `typed_methods` table and recorded a recipe for that span.

## Status

- **Shipped (Phases A + B):** the registry and neutral marshalling seam; `math`/`random`/`time`/`env`/`args`/`fs`/`vec`/`quat`/`json` all migrated onto it; the old `NativeModule` enum deleted; `json.parse::<T>` working end to end.
- **Shipped (extern-types arc):** `ExtType`/`ExternValue` first-class types (`Uuid`, and `FileHandle` migrated off its hand-threaded hosting); extern map/set keys (`Map<Uuid, T>`); the `ExternIo` async seam (`fs.*_async` migrated off its per-backend intercepts, async metadata twins added with zero backend edits).
- **Shipped (crypto arc):** `std.crypto` (digests, HMAC, bcrypt, `random_bytes`) and the incremental `Hasher` — the third extern type, landed with zero backend edits, proving the mutable + host-free corner; `SigType::Union` (`string|bytes` signature positions, mapped onto declared unions).
- **Shipped (http arc):** the seventh `Host` capability (**Network**) — a pure sandbox responder + a real reqwest/rustls client; `std.http` (sync + async verbs, incl. QUERY) returning a `Response` extern type; the async path drives `RealBody::Async` with a real future; and **general optional-param support** (`SigType::Optional`) for registry functions and extern-type methods.
- **Shipped (P-NATIVE):** the ABI (registry vocabulary, `Host` seam, `ExternValue`, `MapKey`, the async `ExternIo`/`Executor` seam, and the Ring 1 primitives) extracted into the lean `noeta-ext-abi` crate; `noeta-stdlib` re-exports it and keeps only the concrete `std` modules + their heavy deps. `noeta-ir`/`noeta-bytecode` now build without the crypto/uuid tree. `Uuid` became a newtype in the process — the orphan-rule pattern any extension uses to expose a foreign type.
- **Shipped (http-server arc):** the **inbound** side of the Network capability (`net_listen`/`net_accept` as an async leaf/`net_reply`), a `Request` extern type, and the `server.serve(port, handler)` construct + `noeta serve` command — a concurrent HTTP server (the sandbox drives a deterministic request script, the real host binds a `TcpListener`).
- **Shipped (higher-order-abi arc):** the `NativeCtx` higher-order dispatch seam (opaque slots + call/poll/drive/advance, generic-over-ctx dispatches so compiled-in extensions monomorphize while the dyn table serves future dynamic loading); `SigType::Fn` + `Var` type variables with checker bind-and-substitute, and `SigType::Generic` extern types with receiver-seeded methods; the Class-3 machinery (per-run retained arena as an enumerable GC root set + `ExtState`); declared arena reads (`arena_getter`) behind extension-synced gates with a per-call-site route cache; extension CLI commands (`ExtCommand`/`CommandCtx`). **The entire hardcoded `Builtin` orchestration family migrated out of core**: `task.sleep`/`all`/`race`/`map_bounded`, `server.serve`, and all of `std.reactive` (graph and all — `Value::Reactive` and both backends' intercepts deleted; the backends no longer depend on `noeta-reactive`), plus the new `std.cell` (`Cell<T>`), plus `noeta serve` out of the CLI enum. Perf-gated throughout: reads at or below the old intercepts (`signal.get` −6%), write-cycle overhead bounded and recorded (higher-order-abi arc ledger, `plans/` git history).
- **Shipped (package-manager Phase 3):** third-party native packages end to end — the registry mechanism moved into `noeta-ext-abi` (install-at-assembly; `noeta-stdlib` is a lazily-seeding facade), the manifest `native` key, the composed-toolchain build + delegation, the raw-buffer ABI (`with_packed`/`with_packed_mut`/`make_packed_like`, `PackedView`, `NativeOut::Scalars`, the fused structural element reads) with the `vec.*_all` family migrated off the **last** per-backend intercepts (perf-gated), external `noeta-<cmd>` binaries, and version `0.1.0` + tags. The registry design test ("could `vec`/`quat` be re-added as a third-party crate with no API change?") is answered by a real out-of-tree proving crate in the composition e2e.
- **Shipped (para-namespace arc):** the first first-party capability to leave `std` for a **non-default package** under a new namespace (`para`). The HTML liveview moved to the pure-Noeta `para.html` package (`packages/para-html/`), and the p2p/local-first stack (`crdt`/`p2p`/`synced`) was physically extracted from `noeta-stdlib` into the `noeta-para-p2p` crate + the native `para-p2p` package (`ParaP2pExtension`, root `para`), installed only when a program depends on it and authorizes it in `[trust]`. `noeta-crdt` and the `ring-p2p` transport stay put; the AOT footprint scan still links the p2panda tree only when `para.p2p`/`para.synced` are imported. **Reactive extension point (the `ReactiveSource` capability):** `para.synced` is a node in the *same* reactive graph as core `std.reactive`, and it participates through the **capability-broker seam** (below) — the `ReactiveSource` trait in the tiny `noeta-reactive-abi` crate, obtained per-run via `noeta_ext_abi::capability::<dyn ReactiveSource>(ctx)`. Three operations over `NodeId`s + arena cells (`create_source` mints a source node, `read_source` is the reactive `.get`, `wake` is the external-change epilogue that reruns dependents), plus the `view.expose` hook. The engine *implements* the trait; the client never touches its representation (the graph, the gate/flush state) and **depends on nothing of `noeta-stdlib`** — the contract crate is the whole surface. This is what lets the p2p package live fully out of `std`.
- **Shipped (para out-of-tree follow-on):** the toolchain groundwork for taking a first-party package fully out-of-tree. **Editions** — `[package] edition` is now a validated, per-package pin recorded in `noeta.lock` and folded into the compiled-bytecode cache key (one edition today; the seam for future language/ABI evolution). **`git` deps track a `branch` or HEAD**, not only a tag, so an in-development or bundled package needs no cut release (the lock still pins the resolved SHA). **The P2p transport left the hosts entirely** — `P2p` is no longer a `Host` capability at all: the host only declares a `real_p2p()` policy, and the `para.p2p` extension owns the backend (loopback broker **or** the real p2panda node, chosen at runtime) in ctx state, reached through the `Send`-handle receive ABI (see [The `Host` capability](#the-host-capability)). The p2panda node moved out of `noeta-host-real` into a leaf crate (`noeta-para-p2p-net`) behind the extension's `ring-p2p` feature, so the core runtime links no iroh/QUIC and a non-`para` `--native` binary sheds the whole tree (~4 MB vs ~27 MB). **Out-of-tree native packages build** — the composed toolchain injects a `[patch]` (`NOETA_TOOLCHAIN_REPO`-configurable) that unifies a package's git-referenced toolchain crates onto the consumer's copy, proven end-to-end for `para-p2p`; the pure-source `para-html` split (standalone repo → registry round-trip) is proven too. In-tree copies stay path deps until the toolchain/registry repos are published (the remaining step).
- **Shipped (capability-broker seam):** cross-extension collaboration became a **trait discovered by type** rather than a hardcoded method or a concrete-type coupling. A provider declares an `ExtCapability` on `Extension::capabilities()`; a consumer calls `capability::<dyn Trait>(ctx)` and gets a handle that owns a clone of the provider's `ExtState` (so it coexists with `&mut dyn NativeCtx`, releasing its engine borrow before every re-entry). The first contract is `noeta-reactive-abi`'s `ReactiveSource` — `para.synced` now reaches the reactive engine through it and depends on **nothing** of `noeta-stdlib` (the old `extension_point` free-function facade is deleted). Recovery is unsafe-free (a `Box<dyn Trait>` erased as `Box<dyn Any>`, downcast back); `TypeId` is consistent within the one linked program the composed toolchain builds. **And the flat `NativeCtx` god-trait was slimmed:** the scheduler's own cross-cutting services — task-local tracing context, the future-completion hook, the hot-reload channel — moved off it into `TaskContext`/`FutureTracing`/`HotReload` sub-traits reached via `ctx.task_context()`/`.future_tracing()`/`.hot_reload()` (backend returns `self` — no lookup, no `Rc` pessimization of the hot scheduler fields the broker would have forced). Two mechanisms, keyed to who owns the state; both let a consumer move out-of-tree without naming the other side. No regression (reactive/host hot paths benchmarked flat).
- **Won't-build (recorded):** *Host-coupled finalizers* — a Rust-side resource in an extern box already finalizes deterministically (RC-zero drops the box, `Drop` runs); a finalizer with `Host` access at free time has no sound access point (values die in release paths carrying no host, including teardown cascades), so buffered types keep explicit `close()`. `ExtType`'s `..DEFAULTS` makes a later `finalizer` field additive if concrete demand appears.
- **Deferred:** publishing the toolchain (and registry) repos — the step that makes a package's true departure from the monorepo portable (a `file://` clone proves the mechanism today, but a committed git-dep needs a reachable public repo; in-tree copies stay path deps until then). The hosted-registry *client* is no longer the gap: registry routing (`[registries]`, `NOETA_REGISTRY_URL`), `noeta publish`/`noeta claim`, and transparency-log verification all ship in `noeta-pm`, with the registry service in its own repo. Also deferred: dynamic loading (the dyn dispatch tables are already in place; every compiled-in extension monomorphizes past them).

## See also

- [Standard-Library Modules](Standard-Library-Modules) — the modules registered through this seam.
- [Concurrency Internals](Concurrency-Internals) — the `Host` capability's role in the deterministic/real split.

## Directives that generate code (`ExtDirective::expand`)

An extension can register **`@`-directives**: `Extension::directives()` returns `ExtDirective` entries, which add a name to the decorator name-space. Resolution runs after the built-in directives and after the tier name-space, so an extension can never shadow either. Each entry declares where it may attach (`sites`), what arguments it takes (`max_args`, `named_keys`), and the prose the editor shows on hover and in completion.

A directive with an `expand` hook does not merely mark a declaration — it **generates its members**. The hook receives the invocation and returns Noeta source:

```rust
fn expand_openapi(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    let spec_path = std::path::Path::new(&ctx.source_dir).join(&ctx.args[0]);
    let reads = vec![spec_path.display().to_string()];
    // Report the read on the ERROR path too: a spec missing today may be written tomorrow, and its
    // appearing is a change that must re-run the hook. `ExpansionError` carries `reads` for exactly
    // this; a bare `Err("msg".into())` leaves them empty when nothing was opened.
    let spec = std::fs::read_to_string(&spec_path)
        .map_err(|e| ExpansionError { message: e.to_string(), reads: reads.clone() })?;
    Ok(Expansion {
        source: methods_for(&spec)?,             // e.g. "fn list_pets(): List<Pet> { … }"
        reads,
    })
}
```

```noeta ignore
@openapi("petstore.yaml")
struct PetStore {
    base_url: string
    // `list_pets`, `get_pet`, … are generated from the spec
}
```

Five things are worth knowing before writing one.

**It is compile-time only, by design.** `@` is the language's codegen half and `#[…]` is the runtime-readable half (see [Attributes and Reflection](Attributes-and-Reflection)). A directive is *not* visible to `attributes_of::<T>()`, and that is deliberate rather than an omission — an extension that wants runtime-visible metadata declares an attribute, and one that wants to consume a resource *dynamically* returns an invocable value instead of reaching for a directive.

**The output is source, not AST.** It goes through the real grammar, so generated code earns the same diagnostics as hand-written code, the ABI stays free of an AST dependency, and the result stays inspectable rather than opaque. What you may emit follows from where the directive attached: members of the decorated declaration, exactly as `@derive` synthesizes methods onto a type. There is no separate notion of output scope — `sites` already answers it.

**Each expansion becomes a real source file.** It is registered in the program's source map under a name that says what caused it — `PetStore ⟨@openapi "petstore.yaml"⟩` — so generated members have true spans. A fault inside a generated method points at that method rather than at the one-line directive that produced a hundred of them. Those sources are what [`noeta expand`](The-CLI#noeta-expand) prints, so a hook can be debugged against its real output — and its output diffed in CI — without having to provoke an error first.

**You must declare every file you read — on the error path too.** `Expansion::reads` (and `ExpansionError::reads`) is the hook's incrementality contract. The compiler cannot discover these by parsing, and it does not simply hand you the file named in your arguments, because a spec routinely pulls in others (an OpenAPI `$ref` into a sibling document) and only the hook knows which. Report every file opened — *including ones that turned out to be missing*, since their appearing later is a change too, and *including when the hook then fails*: the reads survive the `Err`, so a spec that is missing today re-runs the expansion the moment it is written. A hook that under-reports will serve stale members until something unrelated invalidates it. Under `--watch` (`test`/`run`/`serve`), the reported reads are watched alongside the `.noe` sources, so editing (or creating) a spec re-runs the generation; the editor's incremental engine treats a change to one as a full re-check.

**A hook only ever sees a legal invocation.** Placement and the declared argument contract are checked before it runs, so it need not defend against a directive that sat somewhere it does not belong or was called with arguments it never declared. Reading the filesystem is authorized by the package's `[trust]` grant; beyond that, a hook must be a pure function of its `DirectiveCtx` and the files it reports.

Failures are reported as **E0062**, always blamed on the directive rather than on a generated line — the author wrote one line and cannot edit the hundred it produced — with the position inside the generated source carried in the message.

## Derive recipes (`ExtDerive`)

An extension can register **derive recipes**: `Extension::derives()` returns `ExtDerive { name, methods, validate }` entries, and `@derive(<Name>)` on a type synthesizes each declared method as a forward into the extension's registered module function — `fn <name>(a1: dyn, …): dyn { return <handler>(self, a1, …) }`, resolved like an expression tier's native handler (no user import). The handler does its real work natively (typically reflecting over the value); the optional `validate` hook can reject unsuitable type shapes at check time (E0050). std's own `Inspect` (`inspect()` → `json.stringify(self)`) is the dogfood. Names resolve after built-in traits and the program's user traits, so a recipe can never shadow either.
