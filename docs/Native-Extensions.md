# Native Extensions

Native modules (like `math`, `json`, `fs`) are not hardcoded into the runtime — they are registered through one uniform seam, and the core `std` modules are the *dogfooded* extension registered *through* that seam rather than special-cased.

> [!NOTE]
> Since package-manager Phase 3, this seam is **open to third-party packages**: a dependency can ship a Rust crate that registers native modules, types, and CLI commands, statically composed into the consumer's toolchain by cargo. See [Writing a native package](#writing-a-native-package) below.

## Why a registry

Hardcoding native modules created four parallel seams that could drift: a `NativeModule` enum, per-backend `call_vec`/`call_json`/… dispatch, and checker tables of known modules. The registry dismantles all four into one mechanism — and it makes differential agreement *structural*: one shared dispatch function per module, not two mirrored copies. The design test the work held itself to: *could `vec`/`quat` be deleted from core and re-added as a third-party crate with no API change?*

## Two crates: `noeta-native` (the ABI) and `noeta-stdlib` (the batteries)

The contract a native extension implements lives in its own lean crate, **`noeta-native`**: the
[registry] vocabulary (`Extension`/`ExtModule`/`ExtType`/`ExtFn`/`SigType`/`RetTy`/`TypeRecipe`,
`NativeValue`/`NativeOut`/`Scalar`), the `Host` capability seam and its traits, the `ExternValue`
contract (`ExternBox`), `MapKey`, the async `ExternIo`/`Executor` seam, and the dep-free Ring 1
primitives. Its only dependencies are `compact_str`/`equivalent`/`hashbrown` — none of core's
batteries (no crypto, uuid, JSON, or HTTP client). **`noeta-stdlib`** depends on it, re-exports it
(`pub use noeta_native::*` — the `core`/`std` relationship), and layers the concrete `std` modules
and their heavy deps on top. So a third-party extension (and internal mid-end crates like
`noeta-ir`) links the lean ABI, not the whole standard library. Since package-manager Phase 3 this
is the *consumed* boundary too: an out-of-tree entry crate depends on `noeta-native` alone, and the
crates are versioned + git-tagged for the composed shim to pin (see
[Writing a native package](#writing-a-native-package)).

## The seam

The registry vocabulary (`noeta-native`, `registry.rs`; the concrete `std` registration and dispatch
router are in `noeta-stdlib`) is built on a **neutral value-marshalling** layer:

- `NativeValue` — the argument view: `Scalar`, `Str`, `Bytes`, `Object { fields }`, `List`, and so on.
- `NativeOut` — the result view, including the bulk `Scalars(ScalarVec)` form (one typed vector for a whole reduction result — the `Bytes` idea applied to primitive lists).

Two per-backend functions, written once each — `marshal_native_arg(&Value) -> NativeValue` and `materialize_native(NativeOut, …) -> Value` — replace all the duplicated dispatch. A module function is then just a `DispatchFn = fn(&mut dyn Host, &[NativeValue]) -> Result<NativeOut, StdError>`, **shared across both backends** so the differential holds by construction. The `Host` capability (see below) is threaded through so `fs`/`time`/`random`/`env`/`args` migrate too (pure modules ignore it).

Registration is declarative:

```
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
`vec`/`quat` `VecExtension`, `P2pExtension`). Every lookup (`find_module`/`find_type`/`commands`)
iterates the whole registry filtered by root, so the split is invisible to resolution — it's the
**dogfood of the multi-extension registry a package plugs into**: a third-party package registers as
another unit under its own root. Each `ExtModule` declares its `ring: Option<&str>` — the single
source of truth for which Cargo feature gates its heavy native deps in a tailored `noeta build
--native` (`std.http.client` → `ring-http-client`, the ~3 MB reqwest/TLS tree; `None` = always-on
core). The footprint scan reads it off the registry, so there's no hand-maintained module→ring table.

`params` and `ret` use `SigType`, a small signature vocabulary (noeta-stdlib cannot see the checker's `Type`); `noeta-check` maps each `SigType` to a real `Type`, so the registry is the single source of truth that *both* the checker and both backends read. A parameter wrapped in `SigType::Optional(&…)` is **trailing-optional** (`client.get(url, headers?)`): the checker derives the required-argument count from the first `Optional`, and the dispatch reads the slot with `args.get(i)`, supplying its own default when the call omits it — so optional params cost no backend change and no default-value machinery.

## First-class types: `ExtType` and `ExternValue`

An extension contributes a **value type** the way it contributes a module. An `ExtType` declares a reserved name (declaring a same-name user type is E0049), an instance-method signature table, one shared method dispatch, and a `key_capable` flag. The value behavior lives on one trait, `ExternValue` — equality, ordering, hashing, display, clone — and each backend hosts every extern type through a **single** variant (`Payload::Extern` in the VM, `Value::Extern` in the tree-walker), so a new native type touches no backend code at all.

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

The entry crate is an ordinary Rust library that depends on `noeta-native` and exports its extension units as a slice — one crate, any number of units (core's own `std` is six units in one crate):

```rust
use noeta_native::registry::{ExtFn, ExtModule, Extension, /* … */};

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

**Versioning policy (pre-1.0).** The consumed crates (`noeta-native`, `noeta-cli` as a lib, `noeta-stdlib`) are versioned and git-tagged together (`v0.1.0` first). A composed shim pins the toolchain by that tag and cargo unifies the extension's `noeta-native` onto the same source, so compatibility is ordinary source-level semver: **pre-1.0, a minor bump may break extension code**; patch releases are additive. `#[non_exhaustive]` is deliberately not used — the `..DEFAULTS` convention is the additive-evolution mechanism (see the N3.6 audit in `plans/package-manager/phase-3-native.md`).

**Where heavy dependencies belong.** An extension whose implementation needs a heavy native tree should either put the effectful part **behind the `Host` capability seam** (the runtime side, where `noeta build --native` can gate it behind a ring feature — how `std.http`'s reqwest tree stays out of non-http binaries) or accept that its whole crate is the include/exclude unit. Unconditional heavy deps in an always-linked crate cannot be dead-code-eliminated per-program; core's own `crypto`/`id` (~65 KB of sha2/bcrypt/uuid, unconditional in `noeta-stdlib`) are the recorded won't-do-with-trigger example — the extension-unit split makes gating them mechanical if a size budget ever demands it.

## Extension commands

An extension can contribute a CLI subcommand (`ExtCommand`: name, help, typed `ArgSpec`s, and a `run` fn) — the in-process `cargo clippy` model. The CLI augments its clap parser with each registered command (so `noeta --help` lists them with real parsing/validation) and dispatches a matched name to the extension, which drives a narrow `CommandCtx`: load + check + run a program file on the real host, optionally appending a synthesized trailing entry call. `noeta serve` is the proving client — it is `SERVE_COMMAND` in the std extension, whose entry call is the exact same `server.serve(port, fetch)` a program can write directly.

## The `Host` capability

All host-coupled effects — filesystem, clock, PRNG, `env`/`args`, the operating system (`os`: subprocess exec + spawn/lifecycle control + system introspection), entropy, ids, the network, p2p, and the three telemetry signals — go through one `Host` trait (twelve capability traits, blanket-impl'd). Two implementations exist: `SandboxHost` (deterministic in-memory VFS, logical clock, seeded RNG, a **pure network responder**, and a scripted exec command set — what the differential always runs) and `RealHost` (real disk, real env, real subprocesses, per-isolate tokio, and a real reqwest client — what `noeta run` uses, never differential-tested). This is the same "simulate deterministically, deploy real" split as the async executor and isolate scheduler. The network capability (http arc) set the async pattern: `RealHost` overrides `net_spawn` to hand the executor a genuine `RealBody::Async` reqwest future while the sandbox resolves at spawn; `os.exec_async` follows it with a `RealBody::Blocking` subprocess body.

## Case study: `json.parse::<T>`

The motivating consumer is a native function that builds a value of a type named *only at the call site* — something a user genuinely cannot express in-language. The grammar `module.func::<T>(args)` is an atom (`Expr::TypedModuleCall`). The checker resolves `T` into a neutral `TypeRecipe` (scalar / option / list / string-keyed map / declared-order struct), and a shared lowering bakes it into a `TypedModuleCall` IR node the VM transcribes to `Op::TypedModuleCall`. Both backends marshal the arguments, run the shared recursive `json::parse_typed(text, &recipe)`, and materialize the result — the reference interpreter through its real registered type, the VM through a fresh same-name shape (method dispatch is name-keyed) — so they agree by construction.

## Status

- **Shipped (Phases A + B):** the registry and neutral marshalling seam; `math`/`random`/`time`/`env`/`args`/`fs`/`vec`/`quat`/`json` all migrated onto it; the old `NativeModule` enum deleted; `json.parse::<T>` working end to end.
- **Shipped (extern-types arc):** `ExtType`/`ExternValue` first-class types (`Uuid`, and `FileHandle` migrated off its hand-threaded hosting); extern map/set keys (`Map<Uuid, T>`); the `ExternIo` async seam (`fs.*_async` migrated off its per-backend intercepts, async metadata twins added with zero backend edits).
- **Shipped (crypto arc):** `std.crypto` (digests, HMAC, bcrypt, `random_bytes`) and the incremental `Hasher` — the third extern type, landed with zero backend edits, proving the mutable + host-free corner; `SigType::Union` (`string|bytes` signature positions, mapped onto declared unions).
- **Shipped (http arc):** the seventh `Host` capability (**Network**) — a pure sandbox responder + a real reqwest/rustls client; `std.http` (sync + async verbs, incl. QUERY) returning a `Response` extern type; the async path drives `RealBody::Async` with a real future; and **general optional-param support** (`SigType::Optional`) for registry functions and extern-type methods.
- **Shipped (P-NATIVE):** the ABI (registry vocabulary, `Host` seam, `ExternValue`, `MapKey`, the async `ExternIo`/`Executor` seam, and the Ring 1 primitives) extracted into the lean `noeta-native` crate; `noeta-stdlib` re-exports it and keeps only the concrete `std` modules + their heavy deps. `noeta-ir`/`noeta-bytecode` now build without the crypto/uuid tree. `Uuid` became a newtype in the process — the orphan-rule pattern any extension uses to expose a foreign type.
- **Shipped (http-server arc):** the **inbound** side of the Network capability (`net_listen`/`net_accept` as an async leaf/`net_reply`), a `Request` extern type, and the `server.serve(port, handler)` construct + `noeta serve` command — a concurrent HTTP server (the sandbox drives a deterministic request script, the real host binds a `TcpListener`).
- **Shipped (higher-order-abi arc):** the `NativeCtx` higher-order dispatch seam (opaque slots + call/poll/drive/advance, generic-over-ctx dispatches so compiled-in extensions monomorphize while the dyn table serves future dynamic loading); `SigType::Fn` + `Var` type variables with checker bind-and-substitute, and `SigType::Generic` extern types with receiver-seeded methods; the Class-3 machinery (per-run retained arena as an enumerable GC root set + `ExtState`); declared arena reads (`arena_getter`) behind extension-synced gates with a per-call-site route cache; extension CLI commands (`ExtCommand`/`CommandCtx`). **The entire hardcoded `Builtin` orchestration family migrated out of core**: `task.sleep`/`all`/`race`/`map_bounded`, `server.serve`, and all of `std.reactive` (graph and all — `Value::Reactive` and both backends' intercepts deleted; the backends no longer depend on `noeta-reactive`), plus the new `std.cell` (`Cell<T>`), plus `noeta serve` out of the CLI enum. Perf-gated throughout: reads at or below the old intercepts (`signal.get` −6%), write-cycle overhead bounded and recorded (see `plans/higher-order-abi`).
- **Shipped (package-manager Phase 3):** third-party native packages end to end — the registry mechanism moved into `noeta-native` (install-at-assembly; `noeta-stdlib` is a lazily-seeding facade), the manifest `native` key, the composed-toolchain build + delegation, the raw-buffer ABI (`with_packed`/`with_packed_mut`/`make_packed_like`, `PackedView`, `NativeOut::Scalars`, the fused structural element reads) with the `vec.*_all` family migrated off the **last** per-backend intercepts (perf-gated), external `noeta-<cmd>` binaries, and version `0.1.0` + tags. The registry design test ("could `vec`/`quat` be re-added as a third-party crate with no API change?") is answered by a real out-of-tree proving crate in the composition e2e.
- **Won't-build (recorded):** *Host-coupled finalizers* — a Rust-side resource in an extern box already finalizes deterministically (RC-zero drops the box, `Drop` runs); a finalizer with `Host` access at free time has no sound access point (values die in release paths carrying no host, including teardown cascades), so buffered types keep explicit `close()`. `ExtType`'s `..DEFAULTS` makes a later `finalizer` field additive if concrete demand appears.
- **Deferred:** packaged/hermetic *distribution* of the toolchain source (today: workspace path deps or the git+tag fetch), and dynamic loading (the dyn dispatch tables are already in place; every compiled-in extension monomorphizes past them).

## See also

- [Standard-Library Modules](Standard-Library-Modules) — the modules registered through this seam.
- [Concurrency Internals](Concurrency-Internals) — the `Host` capability's role in the deterministic/real split.
