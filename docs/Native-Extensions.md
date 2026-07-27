# Native Extensions

Native modules (like `math`, `json`, `fs`) are not hardcoded into the runtime — they are registered through one uniform seam, and the core `std` modules are themselves an extension registered *through* that seam rather than special-cased.

> [!NOTE]
> This seam is **open to third-party packages**: a dependency can ship a Rust crate that registers native modules, types, and CLI commands, statically composed into the consumer's toolchain by cargo. This page covers the concepts — the registry, the dispatch seams, and the `Host` capability; the author-facing walkthrough (entry crate, manifest, composition, building, publishing) is **[Writing a Native Package](Writing-Native-Packages)**, and the API contract is [Extension Compatibility](Extension-Compatibility).

## Why a registry

Hardcoding native modules created four parallel seams that could drift: a `NativeModule` enum, per-backend `call_vec`/`call_json`/… dispatch, and checker tables of known modules. The registry dismantles all four into one mechanism — and it makes differential agreement *structural*: one shared dispatch function per module, not two mirrored copies. The design test: *could `vec`/`quat` be deleted from core and re-added as a third-party crate with no API change?* (They could — a real out-of-tree proving crate in the composition test suite answers it.)

## Two crates: `noeta-ext-abi` (the ABI) and `noeta-stdlib` (the batteries)

The contract a native extension implements lives in its own lean crate, **`noeta-ext-abi`**: the
[registry] vocabulary (`Extension`/`ExtModule`/`ExtType`/`ExtFn`/`SigType`/`RetTy`/`TypeArgWrap`/`TypeRecipe`,
`NativeValue`/`NativeOut`/`Scalar`), the `Host` capability seam and its traits, the `ExternValue`
contract (`ExternBox`), `MapKey`, the async `ExternIo`/`Executor` seam, and the dep-free Ring 1
primitives. Its only dependencies are `compact_str`/`equivalent`/`hashbrown` — none of core's
batteries (no crypto, uuid, JSON, or HTTP client). **`noeta-stdlib`** depends on it, re-exports it
(`pub use noeta_ext_abi::*` — the `core`/`std` relationship), and layers the concrete `std` modules
and their heavy deps on top. So a third-party extension (and internal mid-end crates like
`noeta-ir`) links the lean ABI, not the whole standard library. This is the *consumed* boundary too:
an out-of-tree entry crate depends on `noeta-ext-abi` alone, and the crates are versioned +
git-tagged for the composed shim to pin (see [Writing a Native Package](Writing-Native-Packages)).

## The seam

The registry vocabulary (`noeta-ext-abi`, `registry.rs`; the concrete `std` registration and dispatch
router are in `noeta-stdlib`) is built on a **neutral value-marshalling** layer:

- `NativeValue` — the argument view: `Scalar`, `Str`, `Bytes`, `Object { fields }`, `List`, and so on.
- `NativeOut` — the result view, including the bulk `Scalars(ScalarVec)` form (one typed vector for a whole reduction result — the `Bytes` idea applied to primitive lists).

Two per-backend functions, written once each — `marshal_native_arg(&Value) -> NativeValue` and `materialize_native(NativeOut, …) -> Value` — replace all the duplicated dispatch. A module function is then just a `DispatchFn = fn(&mut dyn Host, &[NativeValue]) -> Result<NativeOut, StdError>`, **shared across both backends** so the differential holds by construction. The `Host` capability (see below) is threaded through so `fs`/`time`/`random`/`env`/`args` work the same way (pure modules ignore it).

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
iterates the whole registry filtered by root, so the split is invisible to resolution — core `std`
exercises exactly the multi-extension registry a package plugs into: a third-party package registers
as another unit under its own root. The `para` namespace is the first first-party capability to take
that exit for real — the p2p/local-first stack (`ParaP2pExtension`, root `para`) lives outside `std`
as the non-default `para/p2p` package (github.com/noeta-lang/para-p2p), alongside the pure-Noeta
`para.html` liveview package. Each `ExtModule` declares its `ring: Option<&str>` — the single
source of truth for which Cargo feature gates its heavy native deps in a tailored `noeta build
--native` (`std.http.client` → `ring-http-client`, the ~3 MB reqwest/TLS tree; `None` = always-on
core). The footprint scan reads it off the registry, so there's no hand-maintained module→ring table.

`params` and `ret` use `SigType`, a small signature vocabulary (noeta-stdlib cannot see the checker's `Type`); `noeta-check` maps each `SigType` to a real `Type`, so the registry is the single source of truth that *both* the checker and both backends read. A parameter wrapped in `SigType::Optional(&…)` is **trailing-optional** (`client.get(url, headers?)`): the checker derives the required-argument count from the first `Optional`, and the dispatch reads the slot with `args.get(i)`, supplying its own default when the call omits it — so optional params cost no backend change and no default-value machinery.

## First-class types: `ExtType` and `ExternValue`

An extension contributes a **value type** the way it contributes a module. An `ExtType` declares a short display name plus a `namespace`; the type's **identity** is the qualified `namespace.name` (`std.id.Uuid`) — what the checker keys `Type::Named` on and what every value returns from `ExternValue::type_identity` (one pre-joined `&'static` literal, so dispatch compares pointers, never formats). Runtime method dispatch, `is`/`.as<T>()`, map-key capability, and reflection all key on that identity, so two extensions may register the same short name under distinct namespaces (`std.metrics.Counter` and `acme.metrics.Counter` coexist; the registry refuses only a duplicate *qualified* identity at assembly). Humans still see the short name everywhere — diagnostics and `type_of` display strip the namespace, exactly like namespaced user types. A signature that names a shared short name spells it qualified (`SigType::Named("acme.metrics.Counter")`); the checker resolves either spelling. Beyond identity an `ExtType` carries an instance-method signature table, one shared method dispatch, and a `key_capable` flag. The value behavior lives on one trait, `ExternValue` — equality, ordering, hashing, display, clone — and each backend hosts every extern type through a **single** variant (`Payload::Extern` in the VM, `Value::Extern` in the tree-walker), so a new native type touches no backend code at all.

The method dispatch has ONE signature covering the whole {pure, mutable} × {host-free, effectful} matrix: the receiver arrives `&mut` (a pure method just doesn't mutate) and the `Host` is always passed (a pure method just doesn't touch it). Three core types prove the corners: `Uuid` (pure, byte-ordered, key-capable — it can key a `Map`/member a `Set`), `FileHandle` (mutable cursor, fs-effectful methods, not key-capable), and `crypto`'s `Hasher` (mutable but host-free — `update` mutates the receiver through the shared cell without ever touching the `Host`). Effects reach the world only through the `Host`; construction of effectful values stays in module functions (`fs.open`).

## First-class classes: `ExtClass` (a true reference type)

Where an `ExtType` is an opaque handle, an **`ExtClass`** is a real language **`class`** — a *reference type* with **identity** (two bindings alias the same instance; `==` is identity, not structural), **language-visible fields** the program reads/mutates/constructs, full participation in the **RC + cycle collector**, and a **destructor** that runs native cleanup on collection. It is `ExtType` grown up: the same qualified-identity family (`namespace.name`, `use pkg.TheClass` to project + re-root, coexisting with a same-short-named user class), plus fields and construction.

The representation is a real language `Object` with a **class-kind shape** — so identity, reference (aliasing) semantics, RC, and cycle participation come from the object model unchanged; the extension adds no backend collector code. An `ExtClass` declares its `name`, `namespace`, and `fields` (`ExtField { name, ty, is_public, is_mut }`); the checker seeds them exactly like a `.noe` class — `records` (field types), `private_fields` (a non-`pub` field read/set from outside is E0035), `mut_fields` (a non-`mut` field assignment is E0033), and `type_kinds = Class` (reference `==`). A value is produced natively by returning `NativeOut::Instance { class, fields }` (the class-kind twin of `NativeOut::Struct`, which is a *value* struct with no identity), and crosses INTO a dispatch as `NativeValue::Instance`. A pure-data class is also **source-constructible** (`Point { x: 1, y: 2 }`) once imported, exactly like a `.noe` class.

**Native state + destructor.** A class that wraps a Rust resource holds it in a **field typed as an extern handle** (an `ExtType` whose `ExternValue` has a Rust `Drop`). When the object is collected — a last-reference release **or** a destructor-free cycle reclamation — the field's box is dropped and its `Drop` runs the cleanup; this is deterministic and needs no new machinery, because the heap free always drops the payload. (There is deliberately no *host-coupled* finalizer: values die in release paths that carry no host, including teardown cascades, so a finalizer with `Host` access at free time has no sound access point — a native class's destructor is self-contained RAII, the same discipline `FileHandle` uses, and buffered types keep an explicit `close()`.) `std`'s battery declares no `ExtClass`; the `ext_class_seam` fixture is the differential + leak-oracle proof that identity, fields, cycle participation, and destructor firing all hold on both backends.

**`ExtStruct` is the value-type twin** (`Extension::structs()`, fielded-unification): the same `name`/`namespace`/`fields` declaration, but structural equality, copy-on-assign, and no identity or destructor — a native-declared value struct, not a class. Both hooks produce the shared `ExtFielded` type, distinguished only by its `FieldedKind`.

## Native enums: `ExtEnum`

An extension declares **enums** — plain, string-/int-backed, and payload-carrying — seeded eagerly into the checker's `symbols.enums` by qualified identity, so a `match` over one is exhaustive (E0011). Values cross both ways (`NativeOut::Variant` / `NativeValue::Variant`, materialized identically on both backends) and are **source-constructible** (`Hue.Red`, `Tag.Labeled(s)`) once imported. Backed `.value()` is a real typed accessor.

## Async functions: the `ExternIo` seam

An extension implements an **async** function without ever seeing the executor: its dispatch returns *work* (`NativeOut::Spawn(descriptor)`) instead of a value, and the backend tickets the descriptor on its executor and hands back a `Future`. The descriptor has two bodies — `run_sync(host)`, which the deterministic sandbox executor always runs **at spawn** (so an extension's async function is differential-deterministic no matter what its real body does), and an optional real body (a blocking closure for the runtime's blocking pool, or a native future) for true concurrency under `noeta run`. No real body means the real executor degrades to the sync body at spawn — correct, just serial. The `fs.*_async` family is the proving client: its descriptors live in the same registry crate, and adding `exists_async`/`remove_async`/`list_async` touched no backend code.

## Higher-order functions: the `NativeCtx` seam

A plain dispatch is value-in/value-out — it cannot take a **closure argument**, call it back, poll futures, or drive the scheduler. Functions that need those register in a module's **ctx table** instead: the dispatch receives its arguments as opaque **slots** (indices into a per-call table of backend values it never sees) and re-enters the backend through one capability trait, `NativeCtx` — `call` a callable slot, `spawn_io`/`timer`/`poll`/`drive` futures, `advance_tasks`/`advance_clock` the scheduler, plus list access and argument probes. Each backend implements the trait once; the slot table owns the refcount discipline centrally (retain on insert, release on free/drop, arguments borrowed from the caller's registers), so a dispatch structurally cannot leak. The dispatch body stays a single shared `fn`, so the differential holds by construction — extended to orchestration code that was previously mirrored per backend as hardcoded `Builtin`s.

That whole former `Builtin` family now lives here as ordinary registered dispatches: `task.sleep`/`all`/`race`/`map_bounded` (drive loops over `call`/`poll`), `server.serve` (the accept→dispatch→reply loop, including the recover-from-abort pattern: a handler abort becomes a 500 and the loop continues), and all of `std.reactive`.

## Persistent state: the retained arena and `ExtState`

An extension that owns **language values across calls** (reactive's graph; a future ORM or collection type) uses two per-run capabilities. The **retained arena** holds values the extension keeps: `retain(slot) -> Retained` moves a value in, `retained_get`/`retained_set`/`release_retained` read/replace/release it. The structural rule: extension-held values **never live inside extern boxes** (an `ExternValue` is `Send`; backend values are not) — a box carries only plain `Retained` ids, and the values sit backend-side where the refcount discipline, the leak oracle, and the cycle collector see them (the arena is an enumerable root set, released destructor-aware at teardown). **`ExtState`** (`state(key, init)`) holds the extension's own Rust data — the reactive graph is one, storing only arena ids.

Generic extern types ride on the same signature vocabulary: a constructor returns `SigType::Generic("Cell", &[Var(0)])` and method signatures reference the receiver's type arguments as `Var(i)`, so `Cell<int>.set("x")` is a static E0007 with no checker special-casing. Hot accessors can be **declared**: `ExtType::arena_getter` marks a method as a gated arena read ("this method's whole behavior is: return the receiver's retained entry"), and the backend inlines it at the call site behind a route cache while the extension's **read gate** is open — which is how a migrated `signal.get()` measures *faster* than the hardcoded builtin it replaced. The extension closes the gate for exactly the windows where the full dispatch does more (dependency tracking while a body runs; a stale memo), and the tree-walker always takes the full dispatch, so the differential proves fast ≡ full on every fixture.

`std.cell` (`Cell<T>` with `get`/`set`/`update`) is the minimal client of this machinery; `std.reactive` is the full one — graph, flush loop, coalescing, and the E0045 runaway guard are all ordinary Rust in its dispatches. Neither backend knows reactivity exists.

## Cross-extension capabilities: the capability-broker seam

When one extension needs a *service another extension provides* — not the host, another extension — it goes through the **capability broker**. The motivating case: `para.synced`'s CRDT-backed signal *is* a node in the same reactive graph as core `std.reactive`, so it must reach that engine to create its node, subscribe a reader, and wake dependents. The engine lives out-of-reach in another crate's per-run `ExtState`, and `Box<dyn Any>` downcasts only to a *concrete* type — never to a trait — so without a broker the consumer would have to name the engine's private struct (or the engine expose it). The broker turns that into a **trait contract discovered by type**:

- **Contract.** The capability is an object-safe trait in its own small crate — `noeta-reactive-abi`'s `ReactiveSource` (`create_source` / `read_source` / `wake`). Both provider and consumer depend on that crate and on nothing of each other. New capabilities are new such crates; `noeta-ext-abi` never names one.
- **Provide.** The provider declares an `ExtCapability` on its `Extension::capabilities()` — the trait's `TypeId`, the `ExtState` key that backs it, and a `build` thunk that wraps the state as the trait object. `CoreExtension` declares the `ReactiveSource` provider, backed by the same `"std.reactive"` slot its own dispatches use, so reaching the engine either way is the same cell.
- **Consume.** `capability::<dyn ReactiveSource>(ctx)` returns `Some(cap)` when some installed extension provides it (`None` otherwise — an honest "is that engine even loaded?"). The handle **owns a clone of the backing `ExtState`**, so it coexists with `&mut dyn NativeCtx`: each method takes `ctx` and borrows the engine only for its own work, releasing before any re-entry (the flush runs user effects, which re-enter reactive). Recovery is unsafe-free — the provider boxes a `Box<dyn Trait>` (a sized fat pointer) erased as `Box<dyn Any>`, and the consumer downcasts back to exactly that.

The payoff is that `NativeCtx` stops accreting one method per cross-cutting concern: a new collaboration — including between an out-of-tree package and core — is a trait crate plus a declaration, no ABI edit and no side naming the other's types. `TypeId` is consistent within one linked program (the composed toolchain builds everything under one lockfile), which is what makes the by-type lookup sound.

**Sibling mechanism — backend-service sub-traits.** The broker is for one *extension's* state vended to another. The concerns that used to grow `NativeCtx` flat — the task-local tracing context, the future-completion tracing hook, the hot-reload channel — are the *scheduler's own* state exposed to extensions, not an extension's, so they take a lighter form: small `TaskContext` / `FutureTracing` / `HotReload` traits (in `noeta-ext-abi`) reached via `ctx.task_context()` / `ctx.future_tracing()` / `ctx.hot_reload()`, where the backend just returns `self`. No `ExtState`, no `TypeId` lookup, no owned handle — one virtual indirection on cold paths — because a backend service reaching its own hot scheduler fields must not be forced behind a shareable `Rc<RefCell<…>>` the broker would require. Same end (nothing new lands on the flat trait; the sub-traits can move to their own crates when `std.tracing`/`http.serve` go out-of-tree), matched to who owns the state.

## Raw buffers: `with_packed` and the bulk-kernel ABI

A `List<packed>` is stored as one contiguous byte buffer, and a bulk kernel (a SIMD-amenable column reduction, an image transform) wants exactly those bytes — with **zero per-element traffic**. Three ctx capabilities provide it:

- `with_packed(slot, |view, bytes| …)` — borrow the element layout + raw buffer (the `with_extern` shape). The layout arrives as a neutral read-only `PackedView { fields, byte_size, column, count }`, because the backends hold different concrete schema representations — the same reason `NativeValue`/`SigType` exist.
- `with_packed_mut(slot, |view, bytes| …)` — transform the buffer **preserving value semantics**: the callback gets a uniquely-owned copy-on-write buffer (in place only under proven sole ownership), and the transformed list arrives as a fresh slot; the input value is never observably mutated.
- `make_packed_like(like, bytes)` — allocate a result list sharing an existing packed slot's element schema (schemas are backend-interned; the seam names them, never builds them).

The element-wise *fallback* (a boxed, non-packed operand) is expressible in the same shared dispatch through the fused structural reads `object_scalars_at`/`make_object_like_element` (one reused scalar buffer, no per-element slots), and a reduction returns its whole result as one typed vector — `NativeOut::Scalars(ScalarVec::F32(…))` — so the backend converts it in a single pass. The `vec.*_all` family is the proving client: `add_all`/`sub_all`/`scale_all`/`dot_all`/`length_all` were the **last per-backend native intercepts** in either backend; they are now one registered ctx dispatch, perf-gated at or below the old special-cased numbers (`tests/bench/pm-native/`). A third-party crate registering a column kernel for the *consumer's own* `@packed` type is proven end-to-end in the composition test suite.

## Method bundles: `impl vec.Kernels for Px {}`

Raw-buffer kernels as free functions are structurally connected to the data (`vec.dot_all(xs, ys)`
accepts any uniform numeric packed list) — invisible to the checker and the editor. A **method
bundle** is the nominal binding on top, and it is not its own mechanism: a bundle is a
fully-defaulted native **`ExtTrait`** (`Extension::traits()`) carrying a structural
**`self_constraint`** (`PackedConstraint` — the field kinds and arity the implementing type's shape
must satisfy), native-derived **`assoc_types`** (the element-relative return types,
`Self::Wide`/`Self::Float`), and a shared **`dispatch`** answering every defaulted method. Each
`ExtTraitMethod` marks its receiver `Element` — on a value of the bound type — or `Bulk` — on a
`List<T>` of it — and a user type opts in explicitly:

```noe
use std.{vec}

@packed struct Px { x: f32; y: f32; z: f32 }
impl vec.Kernels for Px {}          // constraint checked HERE, at compile time

d  = xs.dot_all(ys)                 // Bulk: methods on List<Px> — same kernel as vec.dot_all
v2 = v.normalize()                  // Element: methods on Px itself
```

`@derive(vec.Kernels)` binds identically — a bundle is `impl`-ed or derived, never both (the
checker dedups and flags a double binding). The binding is what makes the whole toolchain smart:
the impl site validates the shape requirement (a mismatch is a compile-time diagnostic naming
expected vs found — `vec.Kernels` itself accepts any uniform numeric `@packed` shape, every integer
width plus `f32`/`f64`, via `ConstraintArity::Uniform`), method calls type nominally (`SameAsArg(0)`
= the receiver's own type, so `xs.add_all(ys)[0].x` resolves statically), member completion lists
the bound methods, and conflicts are rejected receiver-aware (an `Element` method against the
type's own methods/fields, a `Bulk` method against built-in list methods). Dispatch is
**call-site-resolved**: the checker bakes the `(module, trait)` route into the compiled call — zero
runtime discovery, an empty list receiver works, and the method form measures at parity with the
module-function form (`tests/bench/kernel-methods/`). The flip side: bundle methods are not
reachable through a `dyn` receiver (`dyn` stays the escape hatch; a runtime binding table would be
additive). `std.vec`'s `Kernels`/`SatKernels` are the first clients; a third-party bundle over the
consumer's own packed type is proven through toolchain composition in the CLI e2e.

`ExtTrait` is not only the kernel-bundle mechanism, though — it is the general native-trait seam:
a program `impl`s a plain native trait for its own types and binds on it
(`fn f<T: NativeTrait>(x: T)`) exactly as for a `.noe` trait (an incomplete impl is E0015), and a
native value laundered through `dyn NativeTrait` dispatches to its native method with no new runtime
plumbing. `self_constraint` is what a kernel bundle adds on top — most native traits declare `None`
and are shape-agnostic.

## Composition: how a package's native code reaches the toolchain

A dependency package names an **entry crate** in its manifest (`native = "…"` in `noeta.toml`); the entry crate depends on `noeta-ext-abi` and exports its units as `pub static NOETA_EXTENSIONS`. When the CLI sees a dependency graph with native crates, it generates a small shim crate aggregating every entry crate's units, builds it with cargo (a `[patch]` section collapses every copy of the toolchain crates onto the consumer's own — Rust type identity demands exactly one), caches the composed binary content-addressed, and exec-delegates to it. The composed binary *is* the app's toolchain: the checker sees the extension's signatures, the LSP its completions, the CLI its commands; a pure-Noeta app never touches any of this. Shipped artifacts (`noeta build --exe`/`--native`) compose a lean runtime-only base instead.

The full mechanics — the manifest key, the shim, the `[patch]`/type-identity subtlety, lean-runner composition, feature-gating dev capabilities, building, testing, and publishing — are on **[Writing a Native Package](Writing-Native-Packages)**.

## Extension commands

An extension can contribute a CLI subcommand (`ExtCommand`: name, help, typed `ArgSpec`s, and a `run` fn) — the in-process `cargo clippy` model. The CLI augments its clap parser with each registered command (so `noeta --help` lists them with real parsing/validation) and dispatches a matched name to the extension, which drives a narrow `CommandCtx`: load + check + run a program file on the real host, optionally appending a synthesized trailing entry call. `noeta serve` is the proving client — it is `SERVE_COMMAND` in the std extension, whose entry call is the exact same `server.serve(port, fetch)` a program can write directly.

## The `Host` capability

All host-coupled effects — filesystem, clock, PRNG, `env`/`args`, the console (`std.io`'s stdin/tty/prompt seam), the operating system (`os`: subprocess exec + spawn/lifecycle control + system introspection), entropy, ids, the network, and the three telemetry signals — go through one `Host` trait (twelve mandatory capability traits, blanket-impl'd), plus one **policy** seam, `P2pProvider`: a host declares through `real_p2p() -> Option<RealP2pConfig>` whether **real** peer networking is permitted here (and with what app-id) — `RealHost` returns `Some`, the deterministic hosts the default `None`. Note it hands out **no transport**: no host implements `P2p` at all (that lives in the `para.p2p` extension — see below). Two implementations exist: `SandboxHost` (deterministic in-memory VFS, logical clock, seeded RNG, a **pure network responder**, and a scripted exec command set — what the differential always runs) and `RealHost` (real disk, real env, real subprocesses, per-isolate tokio, and a real reqwest client — what `noeta run` uses, never differential-tested).

**P2p is a capability an *extension* provides, not the host — the whole transport.** Because the p2p stack lives in the non-default `para` package, `P2p` is not an arm of `Host` at all; the transport belongs entirely to the `para.p2p` extension. The extension owns one `P2pBackend` (`Arc<Mutex<dyn P2p + Send>>`) in per-run ctx state (`ExtState`), created on first use from the host's `real_p2p()` policy: the **real p2panda node** (shipped with the package) when the host permits real networking *and* the extension is built with its `ring-p2p` feature, otherwise the deterministic **loopback broker** (`noeta_ext_abi::P2pBroker`, dep-free). Both implement `P2p`; the surface reaches either through one `with_p2p` seam. So **no host implements `P2p` at all** — `RealHost` included — and `noeta-host-real` links no p2panda: the entire iroh/QUIC tree travels with the out-of-tree package (a non-`para` `--native` binary is ~4 MB, a `para` one ~27 MB). The wrinkle the seam solves: the async `p2p.receive` leaf is `Send` while `ExtState` is not, so the backend lives behind a `Send` `Arc<Mutex<…>>` the receive descriptor captures at spawn — the ABI that lets an extension own an async-reachable host capability. This is the same "simulate deterministically, deploy real" split as the async executor and isolate scheduler. The network capability set the async pattern: `RealHost` overrides `net_spawn` to hand the executor a genuine `RealBody::Async` reqwest future while the sandbox resolves at spawn; `os.exec_async` follows it with a `RealBody::Blocking` subprocess body.

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

**The recipe contract.** The grammar `module.func::<T>(args)` is an atom (`Expr::TypedModuleCall`). The checker resolves `T` into a neutral `TypeRecipe` (scalar / unit / option / list / string-keyed map / declared-order struct — *no* enum/class/unconstrained generic, which have no recipe and are a compile-time error at the call), records it at the call site, and a shared lowering bakes it into a `TypedModuleCall` IR node the VM transcribes to `Op::TypedModuleCall`. A struct's fields arrive as `FieldRecipe`s — name, recipe, and a `FieldDefault` saying what an *omitted* field means (required, or a literal default baked in as JSON text), which is how a decode fills a defaulted field without re-entering the program. At dispatch, both backends marshal the arguments, look up the module's `typed_dispatch`, and call it threaded the `&TypeRecipe`:

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

## Directives that generate code (`ExtDirective::expand`)

An extension can register **`@`-directives**: `Extension::directives()` returns `ExtDirective` entries, which add a name to the decorator name-space. Resolution runs after the built-in directives and after the tier name-space, so an extension can never shadow either. Each entry declares where it may attach (`sites`), what arguments it takes (`max_args`, `named_keys`), and the prose the editor shows on hover and in completion.

A directive with an `expand` hook does not merely mark a declaration — it **generates its members**. The hook receives the invocation *and the declaration it decorates* (`DirectiveCtx`: `args`, `named`, `target`, `site`, `source_dir`, `fields`) and returns Noeta source:

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

Six things are worth knowing before writing one.

**A hook can generate from the declaration's *shape*, not only its name.** `DirectiveCtx::fields` is the decorated declaration's members as `(name, declared type spelling)` pairs, in declaration order — a `struct`'s or `class`'s fields; an `enum`'s **variants**, each with its payload spelling as declared (`"(index: int)"` for a named payload, `"(T)"` for a positional one, and the empty string for a variant that carries none); and empty for a `Function`, `Method` or `Trait` site, which declares no typed members. So a validation directive can emit one check per field, a persistence directive one column accessor per field, and a serializer the round-trip pair — none of which is expressible from a name alone:

```rust
fn expand_columns(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    let mut source = String::new();
    for (name, ty) in &ctx.fields {
        source.push_str(&format!("fn {name}_column(): string {{ return \"{name}:{ty}\"; }}\n"));
    }
    Ok(Expansion { source, reads: Vec::new() })
}
```

A spelling is the **declared surface** one, at full fidelity: `List<int>` arrives as `"List<int>"` and never as `"List"`, and `?User` as `"?User"` and never as `"Option<User>"` — a hook writes source, and source is written in the surface language. The one adjustment is that a namespace-qualified identity renders as its short name (`std.id.Uuid` → `Uuid`), because the linker qualifies an imported type before a hook runs and generated code spelling that identity would name something the consumer's file cannot resolve. An unannotated member reports `dyn`. It is the same derivation the checker hands `ExtDerive::validate`, so a recipe and an expansion hook in one extension always see the same declaration the same way.

This does not widen what a hook can see. The declaration's own fields are part of *what the directive was written on*, and they live in the same source text the memoized link is already keyed on — so editing a field re-runs the expansion, exactly as editing the directive's arguments does. What a hook still cannot see is the surrounding program.

**It is compile-time only, by design.** `@` is the language's codegen half and `#[…]` is the runtime-readable half (see [Attributes and Reflection](Attributes-and-Reflection)). A directive is *not* visible to `attributes_of::<T>()`, and that is deliberate rather than an omission — an extension that wants runtime-visible metadata declares an attribute, and one that wants to consume a resource *dynamically* returns an invocable value instead of reaching for a directive.

**The output is source, not AST.** It goes through the real grammar, so generated code earns the same diagnostics as hand-written code, the ABI stays free of an AST dependency, and the result stays inspectable rather than opaque. What you may emit follows from where the directive attached: members of the decorated declaration, exactly as `@derive` synthesizes methods onto a type. There is no separate notion of output scope — `sites` already answers it.

**Each expansion becomes a real source file.** It is registered in the program's source map under a name that says what caused it — `PetStore ⟨@openapi "petstore.yaml"⟩` — so generated members have true spans. A fault inside a generated method points at that method rather than at the one-line directive that produced a hundred of them. Those sources are what [`noeta expand`](The-CLI#noeta-expand) prints, so a hook can be debugged against its real output — and its output diffed in CI — without having to provoke an error first.

**You must declare every file you read — on the error path too.** `Expansion::reads` (and `ExpansionError::reads`) is the hook's incrementality contract. The compiler cannot discover these by parsing, and it does not simply hand you the file named in your arguments, because a spec routinely pulls in others (an OpenAPI `$ref` into a sibling document) and only the hook knows which. Report every file opened — *including ones that turned out to be missing*, since their appearing later is a change too, and *including when the hook then fails*: the reads survive the `Err`, so a spec that is missing today re-runs the expansion the moment it is written. A hook that under-reports will serve stale members until something unrelated invalidates it. Under `--watch` (`test`/`run`/`serve`), the reported reads are watched alongside the `.noe` sources, so editing (or creating) a spec re-runs the generation; the editor's incremental engine treats a change to one as a full re-check.

**`ctx.target` is a bare identifier.** The decorated declaration's name arrives unqualified even when its file declares a `namespace`, because that is the only spelling in scope where the generated members land — a hook can put it straight into a constructor's return type or a struct literal (`fn new(api: Api): {target} { return {target} { … } }`) without unqualifying it first.

**A hook only ever sees a legal invocation.** Placement and the declared argument contract are checked before it runs, so it need not defend against a directive that sat somewhere it does not belong or was called with arguments it never declared. Reading the filesystem is authorized by the package's `[trust]` grant; beyond that, a hook must be a pure function of its `DirectiveCtx` and the files it reports.

Failures are reported as **E0062**, always blamed on the directive rather than on a generated line — the author wrote one line and cannot edit the hundred it produced — with the position inside the generated source carried in the message.

## Derive recipes (`ExtDerive`)

An extension can register **derive recipes**: `Extension::derives()` returns `ExtDerive { name, methods, validate }` entries, and `@derive(<Name>)` on a type synthesizes each declared method as a forward into the extension's registered module function — `fn <name>(a1: dyn, …): dyn { return <handler>(self, a1, …) }`, resolved like an expression tier's native handler (no user import). The handler does its real work natively (typically reflecting over the value); the optional `validate` hook can reject unsuitable type shapes at check time (E0050), reading the deriving type's name and its `(field name, declared type spelling)` pairs — the same shape, from the same derivation, that an expanding directive gets as [`DirectiveCtx::fields`](#directives-that-generate-code-extdirectiveexpand). std's own `Inspect` (`inspect()` → `json.stringify(self)`) is the reference example. Names resolve after built-in traits and the program's user traits, so a recipe can never shadow either.

## Current state

- **The ABI is its own lean crate**: `noeta-ext-abi` carries the whole registration + dispatch contract (registry vocabulary, `Host` seam, `ExternValue`, `MapKey`, `NativeCtx`, the async `ExternIo`/`Executor` seam, the channel semantics, the p2p broker/policy, the Ring 1 primitives); `noeta-stdlib` re-exports it and layers the concrete `std` modules on top.
- **All of `std` goes through the seam**: every native module (`math`, `time`, `fs`, `json`, `vec`/`quat`, `crypto`, `id`, `http` client + server, `io`, `os`, `task`, `cell`, `reactive`, …) is a registered extension unit; neither backend contains a native-module enum, hardcoded builtins, or per-module intercepts.
- **Declaration surfaces**: modules and functions (optional params, unions, generics, call-site-typed functions/methods), extern types (`ExtType`/`ExternValue`), enums (`ExtEnum`), classes and structs (`ExtClass`/`ExtStruct`), traits (`ExtTrait`, including kernel bundles), CLI commands, tiers, attributes, directives (`expand`), derives, body formatters, and broker capabilities.
- **Higher-order and stateful extensions work**: the `NativeCtx` slot seam, per-run `ExtState`, the retained arena (an enumerable GC root set), declared arena reads, and the raw packed-buffer kernel ABI (`with_packed`/`with_packed_mut`/`make_packed_like`, `NativeOut::Scalars`).
- **Third-party packages compose end to end**: the manifest `native` key, the composed-toolchain build with `[patch]` unification, lean shipped-artifact composition, and external `noeta-<cmd>` binaries.
- **Out-of-tree is production reality**: the seven `para/*` packages live in their own repos under github.com/noeta-lang and are published on the hosted registry at [registry.noeta.dev](https://registry.noeta.dev) (keyless Sigstore-signed, transparency-logged), resolved as ordinary registry dependencies.
- **Deliberately not built**: host-coupled finalizers — a Rust resource in an extern box already finalizes deterministically via `Drop`, and a finalizer with `Host` access at free time has no sound access point, so buffered types keep explicit `close()`.
- **Deferred**: dynamic loading — the dyn dispatch tables are in place, but every extension today is compiled in and monomorphizes past them.

## See also

- [Writing a Native Package](Writing-Native-Packages) — the author-facing walkthrough: entry crate, manifest, composition, building, testing, publishing.
- [Extension Compatibility](Extension-Compatibility) — the API contract for native package authors: the stable surface, what is not stable, and the pre-1.0 policy.
- [Standard library reference](Std) — the modules registered through this seam.
- [Concurrency Internals](Concurrency-Internals) — the `Host` capability's role in the deterministic/real split.
