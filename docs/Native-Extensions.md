# Native Extensions

Every native module, `math` and `json` and `fs` among them, is registered through one uniform seam. The core `std` modules are an extension registered through that same seam.

> [!NOTE]
> This seam is **open to third-party packages**: a dependency can ship a Rust crate that registers native modules, types, and CLI commands, statically composed into the consumer's toolchain by cargo. This page covers the registry, the dispatch seams, and the `Host` capability. The author-facing walkthrough (entry crate, manifest, composition, building, publishing) is **[Writing a Native Package](Writing-Native-Packages)**, and the API contract is [Extension Compatibility](Extension-Compatibility).

## The two crates

A native extension implements a contract that lives in **`noeta-ext-abi`**, a lean crate holding the registry vocabulary (`Extension`, `ExtModule`, `ExtType`, `ExtFn`, `SigType`, `RetTy`), the value contracts (`NativeValue`, `ExternValue`, `MapKey`, `Scalar`), the `Host` capability seam, the async `ExternIo`/`Executor` seam, and the Ring 1 primitives. It depends on `compact_str`, `equivalent` and `hashbrown`, and on none of the standard library's batteries.

**`noeta-stdlib`** depends on that crate, re-exports it, and layers the concrete `std` modules and their heavy dependencies on top. The relationship is `core` to `std`.

Depend on `noeta-ext-abi` alone. An out-of-tree entry crate names it by version range from crates.io, and composition redirects that to the consumer's own copy. See [Writing a Native Package](Writing-Native-Packages).

Everything the toolchain knows about a native module arrives through this one registry, so a module registered by a third-party crate reaches the checker and both backends exactly as `std.vec` does.

## The seam

The registry vocabulary lives in `noeta-ext-abi`'s `registry` module, and the concrete `std` registration and dispatch router in `noeta-stdlib`. Values cross the seam through a **neutral marshalling** layer: `NativeValue` is the argument view (`Scalar`, `Str`, `Bytes`, `Object { fields }`, `List`, and so on) and `NativeOut` the result view, including the bulk `Scalars(ScalarVec)` form, one typed vector for a whole reduction result.

Conversion is per backend and written once there, so a module function is a `DispatchFn = fn(&mut dyn Host, &[NativeValue]) -> Result<NativeOut, StdError>` **shared across both backends**, and the differential holds by construction. The `Host` capability (see below) is threaded through so `fs`/`time`/`random`/`env`/`args` work the same way, and pure modules ignore it.

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

### Roots and registry units

Every lookup is filtered by root, so the split of a root into several units is invisible to resolution. Core's `std` is several in-tree `Extension` units sharing the `"std"` root: `CoreExtension` (always-on) plus one per capability with a separable identity (`HttpExtension`, `CryptoExtension`, `IdExtension`, and the `vec`/`quat` `VecExtension`). A third-party package registers as another unit under its own root.

The `para` namespace is a first-party root outside `std`. The p2p and local-first stack (`ParaP2pExtension`, root `para`) ships as the non-default `para/p2p` package at github.com/noeta-lang/para-p2p, alongside the pure-Noeta `para.html` liveview package.

### Signatures and rings

`params` and `ret` use `SigType`, a small signature vocabulary (`noeta-stdlib` cannot see the checker's `Type`). `noeta-check` maps each `SigType` to a real `Type`, so the registry is the single source of truth that *both* the checker and both backends read.

A parameter wrapped in `SigType::Optional(&…)` is **trailing-optional** (`client.get(url, headers?)`). The checker derives the required-argument count from the first `Optional`, and the dispatch reads the slot with `args.get(i)`, supplying its own default when the call omits it.

Each `ExtModule` declares its `ring: Option<&str>`, the single source of truth for which Cargo feature gates its heavy native dependencies in a tailored `noeta build --native`. `std.http.client` names `ring-http-client`, the reqwest and TLS tree; `None` means always-on core. The footprint scan reads the ring off the registry.

## What an extension declares

Every surface below is a defaulted method on `Extension`, so an extension declares the ones it needs and stays source-compatible as more are added.

| Surface | Hook | Where it is covered |
|---|---|---|
| Modules and their functions | `modules` | The seam, above |
| First-class value types | `types` | First-class types |
| Enums | `enums` | Native enums |
| Classes and value structs | `classes`, `structs` | First-class classes |
| Traits, including kernel bundles | `traits` | Method bundles |
| CLI subcommands | `commands` | Extension commands |
| Dev tiers and their native runners | `tiers`, `tier_runners` | [Extending Tiers](Extending-Tiers) |
| Prelude attributes | `attributes` | [Attributes and Reflection](Attributes-and-Reflection) |
| `@`-directives, including `expand` hooks | `directives` | Directives that generate code |
| Derive recipes | `derives` | Derive recipes |
| Tier-body formatters for `noeta fmt` | `body_formatters` | [Writing a Native Package](Writing-Native-Packages) |
| Services vended to other extensions | `capabilities` | Cross-extension capabilities |

## First-class types: `ExtType` and `ExternValue`

An extension contributes a value type the way it contributes a module. An `ExtType` declares a short display name and a `namespace`, and the type's identity is the qualified `namespace.name`, such as `std.id.Uuid`.

That identity is what the checker keys `Type::Named` on, and what every value returns from `ExternValue::type_identity`. It is one pre-joined `&'static` literal, so dispatch compares pointers.

Runtime method dispatch, `is`, `.as<T>()`, map-key capability and reflection all key on the same identity. Two extensions may therefore register the same short name under different namespaces: `std.metrics.Counter` and `acme.metrics.Counter` coexist, and the registry refuses only a duplicate qualified identity.

Method dispatch has one signature covering every combination of pure or mutating and host-free or effectful. The receiver arrives `&mut`, and the `Host` is always passed; a pure method simply does not use them. Three core types sit at the corners:

| Type | Shape |
|---|---|
| `Uuid` | pure, byte-ordered, key-capable, so it can key a `Map` or member a `Set` |
| `FileHandle` | mutable cursor, fs-effectful methods, not key-capable |
| `crypto`'s `Hasher` | mutable but host-free, since `update` mutates the receiver through the shared cell without touching the `Host` |

Effects reach the world only through the `Host`. Constructing an effectful value stays in a module function such as `fs.open`.

## First-class classes: `ExtClass` (a true reference type)

An **`ExtClass`** is a real language **`class`**: a *reference type* with **identity** (two bindings alias the same instance, and `==` compares identity rather than structure), **language-visible fields** the program reads, mutates and constructs, full participation in the **RC and cycle collector**, and a **destructor** that runs native cleanup on collection. It shares the qualified-identity family of an `ExtType` (`namespace.name`, `use pkg.TheClass` to project and re-root, coexisting with a same-short-named user class), and adds fields and construction.

The value is an ordinary language object carrying a class-kind shape, so identity, aliasing, RC and cycle participation come from the object model and the extension adds no backend collector code.

An `ExtClass` declares its `name`, `namespace`, and `fields` (`ExtField { name, ty, is_public, is_mut }`), which the checker seeds exactly like a `.noe` class's:

| `ExtField` | What the checker enforces |
|---|---|
| `ty` | the field's declared type |
| `is_public` | a non-`pub` field read or set from outside the class is E0035 |
| `is_mut` | an assignment to a non-`mut` field is E0033 |

A value is produced natively by returning `NativeOut::Instance { class, fields }` and crosses into a dispatch as `NativeValue::Instance`. A pure-data class is also **source-constructible** (`Point { x: 1, y: 2 }`) once imported, exactly like a `.noe` class.

**`ExtStruct` is the value-type twin** (`Extension::structs()`): the same `name`, `namespace` and `fields` declaration, produced as `NativeOut::Struct`, with structural equality, copy-on-assign, no identity and no destructor. Both hooks produce the shared `ExtFielded` type, distinguished by its `FieldedKind`.

### Native state and destructors

A class that wraps a Rust resource holds it in a **field typed as an extern handle**, an `ExtType` whose `ExternValue` has a Rust `Drop`. When the object is collected, by a last-reference release or by a destructor-free cycle reclamation, the field's box is dropped and its `Drop` runs the cleanup. The heap free always drops the payload, so that is deterministic and needs no new machinery.

A destructor is self-contained RAII, the discipline `FileHandle` uses. There is no *host-coupled* finalizer: values die in release paths that carry no host, teardown cascades included. A buffered type therefore keeps an explicit `close()`.

## Native enums: `ExtEnum`

An extension declares **enums** in three forms (plain, string- or int-backed, and payload-carrying), seeded eagerly into the checker's `symbols.enums` by qualified identity, so a `match` over one is exhaustive (E0011). Values cross both ways (`NativeOut::Variant` and `NativeValue::Variant`, materialized identically on both backends) and are **source-constructible** (`Hue.Red`, `Tag.Labeled(s)`) once imported. Backed `.value()` is a real typed accessor.

## Async functions: the `ExternIo` seam

An extension implements an **async** function without seeing the executor. Its dispatch returns *work*, `NativeOut::Spawn(descriptor)`, instead of a value, and the backend tickets the descriptor on its executor and hands back a `Future`.

The descriptor has two bodies. `run_sync(host)` is what the deterministic sandbox executor always runs **at spawn**, so an extension's async function is differential-deterministic no matter what its real body does. The optional real body, a blocking closure for the runtime's blocking pool or a native future, gives true concurrency under `noeta run`; with no real body, the real executor runs the sync body at spawn, correctly but serially.

The `fs.*_async` family is the proving client, with its descriptors declared in the same registry crate as the synchronous functions.

## Higher-order functions: the `NativeCtx` seam

A function that takes a **closure argument**, calls it back, polls futures, or drives the scheduler registers in a module's **ctx table** rather than its ordinary function table. Its arguments arrive as opaque **slots**, indices into a per-call table of backend values it never sees, and it re-enters the backend through one capability trait, `NativeCtx`.

`NativeCtx` can `call` a callable slot, `call_method` a *named* method on a receiver, `spawn_io`/`timer`/`poll`/`drive` futures, `advance_tasks`/`advance_clock` the scheduler, `render` a value to its canonical display string, and `bytes_of` a `bytes` slot, alongside list access and argument probes.

Each backend implements the trait once, and the slot table owns the refcount discipline centrally (retain on insert, release on free or drop, arguments borrowed from the caller's registers), so a dispatch structurally cannot leak. The dispatch body stays a single shared `fn`, so the differential holds by construction over orchestration code as well.

`view`'s deep projection renders a `bytes` value as a **summary string** (`"<12 bytes>"`), so a display or JSON path can never choke on binary. **`bytes_of`** returns the payload itself, for a native that needs it, such as a value's wire encoding.

**`call_method`** takes a receiver plus a method name, where `call` takes a callable *value*, and answers `Ok(None)` when the receiver declares no such method. It is how a native reaches a method a **user type** declared: the extension holds the values and the name from its own contract, never a closure. The motivating case is a trait an extension declares and a user type implements.

Ordinary registered ctx dispatches cover `task.sleep`/`all`/`race`/`map_bounded` (drive loops over `call` and `poll`), `server.serve` (the accept, dispatch and reply loop, including the recover-from-abort pattern where a handler abort becomes a 500 and the loop continues), and all of `std.reactive`.

## Persistent state: the retained arena and `ExtState`

An extension that owns **language values across calls** (reactive's graph, an ORM, a collection type) uses two per-run capabilities.

The **retained arena** holds the values the extension keeps: `retain(slot) -> Retained` moves a value in, and `retained_get`/`retained_set`/`release_retained` read, replace and release it. Those values live backend-side and never inside extern boxes, because an `ExternValue` is `Send` and backend values are not. A box carries only plain `Retained` ids, while the values sit where the refcount discipline, the leak oracle and the cycle collector see them. The arena is an enumerable root set, released destructor-aware at teardown.

**`ExtState`** (`state(key, init)`) holds the extension's own Rust data. The reactive graph is one, storing only arena ids.

Generic extern types ride on the same signature vocabulary. A constructor returns `SigType::Generic("Cell", &[Var(0)])` and method signatures reference the receiver's type arguments as `Var(i)`, so `Cell<int>.set("x")` is a static E0007 with no checker special-casing.

Hot accessors can be **declared**. `ExtType::arena_getter` marks a method as a gated arena read, meaning its whole behavior is to return the receiver's retained entry, and the backend inlines it at the call site behind a route cache while the extension's **read gate** is open. `signal.get()` is declared this way. The extension closes the gate for exactly the windows where the full dispatch does more, such as dependency tracking while a body runs, or a stale memo. The tree-walker always takes the full dispatch, so the differential proves the fast path and the full one agree on every fixture.

`std.cell` (`Cell<T>` with `get`/`set`/`update`) is the minimal client of this machinery. `std.reactive` is the full one: its graph, flush loop, coalescing and E0045 runaway guard are ordinary Rust in its dispatches, and neither backend knows reactivity exists.

## Cross-extension capabilities: the capability-broker seam

When one extension needs a service *another extension provides*, it reaches it through the **capability broker**, which turns that service into a trait contract discovered by type. The motivating case is `para.synced`'s CRDT-backed signal: it *is* a node in the same reactive graph as core `std.reactive`, so it must reach that engine, living in another crate's per-run `ExtState`, to create its node, subscribe a reader, and wake dependents.

| Side | What it writes |
|---|---|
| Contract | an object-safe trait in its own small crate, `noeta-reactive-abi`'s `ReactiveSource` (`create_source` / `read_source` / `wake`). Both sides depend on that crate and on nothing of each other. |
| Provide | an `ExtCapability` on `Extension::capabilities()`: the trait's `TypeId`, the `ExtState` key backing it, and a `build` thunk wrapping that state as the trait object. |
| Consume | `capability::<dyn ReactiveSource>(ctx)`, which answers `Some(cap)` when some installed extension provides it and `None` otherwise, so "is that engine even loaded?" is a question a dispatch can ask. |

`CoreExtension` declares the `ReactiveSource` provider, backed by the same `"std.reactive"` slot its own dispatches use, so reaching the engine either way is the same cell. New capabilities are new such crates, and `noeta-ext-abi` never names one.

A capability handle coexists with `&mut dyn NativeCtx`: each method takes `ctx` and borrows the engine only for its own work, releasing before any re-entry, which the flush needs because it runs user effects that re-enter reactive.

A new collaboration, including one between an out-of-tree package and core, is a trait crate plus a declaration, with no ABI edit and neither side naming the other's types. The by-type lookup is sound because `TypeId` is consistent within one linked program, and the composed toolchain builds everything under one lockfile.

**Backend-service sub-traits.** Three concerns belong to the *scheduler's own* state rather than to an extension's: the task-local tracing context, the future-completion tracing hook, and the hot-reload channel. Each is a small trait in `noeta-ext-abi` (`TaskContext`, `FutureTracing`, `HotReload`) reached through `ctx.task_context()`, `ctx.future_tracing()` and `ctx.hot_reload()`, where the backend returns `self`. There is no `ExtState`, no `TypeId` lookup and no owned handle.

## Raw buffers: `with_packed` and the bulk-kernel ABI

A `List<packed>` is stored as one contiguous byte buffer, and a bulk kernel (a SIMD-amenable column reduction, an image transform) wants exactly those bytes, with **zero per-element traffic**. Three ctx capabilities provide it:

- `with_packed(slot, |view, bytes| …)` borrows the element layout and raw buffer. The layout arrives as a neutral read-only `PackedView { fields, byte_size, column, count }`, because the backends hold different concrete schema representations.
- `with_packed_mut(slot, |view, bytes| …)` transforms the buffer **preserving value semantics**: the callback gets a uniquely-owned copy-on-write buffer (in place only under proven sole ownership), and the transformed list arrives as a fresh slot. The input value is never observably mutated.
- `make_packed_like(like, bytes)` allocates a result list sharing an existing packed slot's element schema. Schemas are backend-interned, and the seam names them rather than building them.

The element-wise *fallback* for a boxed, non-packed operand is expressible in the same shared dispatch, through the fused structural reads `object_scalars_at` and `make_object_like_element` (one reused scalar buffer, no per-element slots). A reduction returns its whole result as one typed vector, `NativeOut::Scalars(ScalarVec::F32(…))`, so the backend converts it in a single pass.

The `vec.*_all` family is the proving client: `add_all`/`sub_all`/`scale_all`/`dot_all`/`length_all` are one registered ctx dispatch rather than a per-backend intercept, perf-gated against `tests/bench/pm-native/`. A third-party crate registering a column kernel for the *consumer's own* `@packed` type is proven end-to-end in the composition test suite.

## Method bundles: `impl vec.Kernels for Px {}`

A **method bundle** binds a kernel set to a type nominally, so that the checker and the editor see it. A bundle is a fully-defaulted native **`ExtTrait`** (`Extension::traits()`) carrying a structural **`self_constraint`** (`PackedConstraint`, the field kinds and arity the implementing type's shape must satisfy), native-derived **`assoc_types`** (the element-relative return types `Self::Wide` and `Self::Float`), and a shared **`dispatch`** answering every defaulted method.

Each `ExtTraitMethod` marks its receiver `Element`, for a value of the bound type, or `Bulk`, for a `List<T>` of it. A user type opts in explicitly:

```noeta ignore
use std.{vec}

@packed struct Px { x: f32; y: f32; z: f32 }
impl vec.Kernels for Px {}          // constraint checked HERE, at compile time

d  = xs.dot_all(ys)                 // Bulk: methods on List<Px>
v2 = v.normalize()                  // Element: methods on Px itself
```

`vec.Kernels` binds the whole vector-math set:

| Methods | Element form, on a value | Bulk `*_all` form, on `List<Self>` |
|---|---|---|
| `add` `sub` `scale` `min` `max` `abs` | lane by lane, returning `Self` | one pass over the packed buffer, returning `List<Self>` |
| `dot` `length` | across the components, returning `Self::Wide` / `Self::Float` | `List<Self::Wide>` / `List<Self::Float>`, one result per element |
| `cross` `lerp` `clamp` `reflect` `normalize` | across the components, returning `Self` | none |
| `distance` | across the components, returning `Self::Float` | none |

A bulk form exists to stream one operation over a whole packed buffer in a single pass. The last two rows read a whole value at a time, so `xs.map(fn(v) => v.normalize())` costs what a `*_all` would and there is nothing to add.

Two narrowings apply, each by what the operation means rather than by width: `cross` needs exactly three components, and `normalize` and `reflect` need a float element. `distance` and `lerp` compute in `Self::Float` and convert back, so an unsigned difference cannot wrap and an integer interpolation keeps its fraction until the closing round, which is nearest, half away from zero, saturating at the element's bounds. The lane ops instead wrap the way `+` does on the same integers.

### Associated types come from the element

`impl vec.Kernels for Px {}` is an empty binding, yet `dot` returns `Self::Wide` and `length` returns `Self::Float`, types *derived* from the implementing type's packed element rather than written by the author. An `f32` element makes both `f32`. An `i8` element widens `dot` to `int`, so a cross-lane sum cannot silently wrap, and promotes `length` to `float`. The derivations are a closed set, `Element`, `Widen` and `FloatPromote`, and every native associated type names one.

In a trait written in `.noe`, where the implementor writes the binding, a [type parameter](Generics-and-Traits) is the mechanism instead. Declaring an associated type in source is refused, with the type parameter named as the fix.

### What the binding buys

`@derive(vec.Kernels)` binds identically. A bundle is `impl`-ed or derived, never both, and the checker dedups and flags a double binding.

The binding is what makes the whole toolchain smart about the type. The impl site validates the shape requirement, and a mismatch is a compile-time diagnostic naming expected against found. (`vec.Kernels` itself accepts any uniform numeric `@packed` shape, every integer width plus `f32` and `f64`, through `ConstraintArity::Uniform`.)

Method calls type nominally, since `SameAsArg(0)` is the receiver's own type, so `xs.add_all(ys)[0].x` resolves statically. Member completion lists the bound methods. Conflicts are rejected receiver-aware: an `Element` method against the type's own methods and fields, a `Bulk` method against built-in list methods.

Dispatch is **call-site-resolved**. The checker bakes the `(module, trait)` route into the compiled call, so there is no runtime discovery, an empty list receiver works, and the method form measures at parity with the module-function form (`tests/bench/kernel-methods/`). Bundle methods are reachable on a statically known receiver; a `dyn` receiver stays the escape hatch and does not carry them.

`std.vec` declares `Kernels` and `SatKernels`. A third-party bundle over the consumer's own packed type is proven through toolchain composition in the CLI e2e.

### `ExtTrait` is the general native-trait seam

A program `impl`s a plain native trait for its own types and binds on it (`fn f<T: NativeTrait>(x: T)`) exactly as for a `.noe` trait, an incomplete impl is E0015, and a native value laundered through `dyn NativeTrait` dispatches to its native method with no new runtime plumbing. Most native traits declare `self_constraint: None` and are shape-agnostic; `self_constraint` is what a kernel bundle adds on top.

## Composition: how a package's native code reaches the toolchain

A dependency package names an **entry crate** in its manifest (`native = "…"` in `noeta.toml`). The entry crate depends on `noeta-ext-abi` and exports its units as `pub static NOETA_EXTENSIONS`.

When the CLI sees a dependency graph with native crates, it generates a small shim crate aggregating every entry crate's units, builds it with cargo, caches the composed binary content-addressed, and exec-delegates to it. A `[patch]` section collapses every copy of the toolchain crates onto the consumer's own, which is what Rust type identity demands.

The composed binary *is* the app's toolchain: the checker sees the extension's signatures, the LSP its completions, the CLI its commands. A pure-Noeta app never touches any of this, and shipped artifacts (`noeta build --exe` and `--native`) compose a lean runtime-only base instead.

**[Writing a Native Package](Writing-Native-Packages)** carries the full mechanics: the manifest key, the shim, the `[patch]` and type-identity subtlety, lean-runner composition, feature-gating dev capabilities, building, testing, and publishing.

## Extension commands

An extension can contribute a CLI subcommand (`ExtCommand`: name, help, typed `ArgSpec`s, and a `run` fn), on the in-process `cargo clippy` model. The CLI augments its clap parser with each registered command, so `noeta --help` lists them with real parsing and validation, and dispatches a matched name to the extension, which drives a narrow `CommandCtx`: load, check and run a program file on the real host, optionally appending a synthesized trailing entry call. `noeta serve` is the proving client, `SERVE_COMMAND` in the std extension, whose entry call is the same `server.serve(port, fetch)` a program can write directly.

**std's own verbs are declared this way too.** `noeta test`, `noeta bench` and `noeta doc` are `ExtCommand`s registered from the `std` root (`noeta-cli`'s `tier_runner` unit, where their native runners live), so nothing about them is special-cased in the binary. std is registered without a grant because it ships with the toolchain, and a [`[trust.commands]`](Manifest#trustcommands--contributed-subcommands) binding under one of those names **replaces** it, with the replacement owning the verb's arguments and help. A test framework, a doc generator or a benchmark harness becomes `noeta test` this way, rather than `noeta yourframework-test`.

An `ArgSpec` covers required and defaulted positionals, `--flag` booleans, string, int and float options with or without defaults, repeatable strings (`--name a --name b`), and an optional one-letter `short` alias (`-j` for `--jobs`, ignored on a positional, which has no flag to alias). Spread `..ArgSpec::DEFAULTS` rather than naming every field, so a later optional field lands in the ABI instead of in your literal. Relations between arguments, "this one needs that one", belong in the command body, which is where the message can say what to do about it.

## The `Host` capability

All host-coupled effects go through one `Host` trait: the filesystem, the clock, the PRNG, `env` and `args`, the console (`std.io`'s stdin, tty and prompt seam), the operating system (`os`, covering subprocess exec, spawn and lifecycle control, and system introspection), entropy, ids, the network, and the three telemetry signals. Each is its own capability trait, and `Host` is blanket-implemented for any type that implements them all.

The native backends run on two of them:

| Host | What it is |
|---|---|
| `SandboxHost` | Deterministic in-memory VFS, logical clock, seeded RNG, a pure network responder, and a scripted exec command set. What the differential always runs. |
| `RealHost` | Real disk, real env, real subprocesses, per-isolate tokio, and a real reqwest client. What `noeta run` uses, never differential-tested. |

The network follows that split: `RealHost` overrides `net_spawn` to hand the executor a genuine `RealBody::Async` reqwest future while the sandbox resolves at spawn, and `os.exec_async` uses a `RealBody::Blocking` subprocess body.

The wasm targets bring their own. `WasiHost` (`crates/noeta-wasi-host`) backs a `wasi:http` component with the capabilities WASI exposes, covered in [Edge Deployment](Edge-Deployment), and `BrowserHost` (`crates/noeta-playground`) backs the playground's real-host mode over the embedder's browser APIs, covered in [WebAssembly and the Edge](WebAssembly-and-the-Edge).

### Peer networking is an extension capability

The whole peer transport is a capability the `para.p2p` *extension* provides, since that package is not a default dependency, so no host implements `P2p` and it is not an arm of `Host`. What a host declares instead is one **policy**, `P2pProvider::real_p2p() -> Option<RealP2pConfig>`, saying whether **real** peer networking is permitted here and with what app-id. `RealHost` returns `Some`, and the deterministic hosts `None`.

The extension owns one `P2pBackend` (`Arc<Mutex<dyn P2p + Send>>`) in per-run ctx state (`ExtState`), created on first use from that policy. The policy picks one of two backends, both implementing `P2p` behind one `with_p2p` seam: the **real p2panda node**, shipped with the package, when the host permits real networking *and* the extension is built with its `ring-p2p` feature, and the deterministic **loopback broker** (`noeta_ext_abi::P2pBroker`, dep-free) otherwise. It is the same simulate-deterministically, deploy-real split as the async executor and the isolate scheduler.

So `noeta-host-real` links no p2panda, and the entire iroh and QUIC tree travels with the out-of-tree package. A non-`para` `--native` binary is ~4 MB, a `para` one ~27 MB.

## Call-site-typed functions: `module.func::<T>(args)`

Some native functions build a value of a type named *only at the call site*, such as `json.parse::<Point>(text)`, which a user cannot express in-language. Any extension can declare one, and the mechanism is registry-driven rather than `json`-hardcoded.

**Declaration.** A call-site-typed function lives in a **separate table** from ordinary functions, `ExtModule::typed_functions`, dispatched by `ExtModule::typed_dispatch`, because the turbofish form `f::<T>(x)` is a distinct call surface from a plain `f(x)`. The two may legitimately share a name: `json.parse` is both a dynamic `parse(text): dyn` in `functions` and a typed `parse::<T>: T` in `typed_functions`. Each entry declares `RetTy::TypeArg(wrap)`, where the `TypeArgWrap` says how the turbofish `T` is wrapped in the declared result:

| `TypeArgWrap` | Declared result |
|---|---|
| `Plain` | `T` itself, the aborting door: `json.parse::<T>(): T` |
| `Option` | `Option<T>` |
| `Result(SigType)` | `Result<T, E>` where `E` is the named error type, the recoverable door: `json.try_parse::<T>(): Result<T, JsonError>` |

The checker reads the wrap to type the call, and validates arguments against the declared `params` with the ordinary native-argument machinery. A wrong-arity or wrong-typed argument is the same static `E0007` a plain call gets, and a turbofish on an unknown or non-call-site-typed function is a clear `E0005`.

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

### The recipe contract

The checker resolves the turbofish `T` into a neutral **`TypeRecipe`**, one of a scalar, unit, option, list, string-keyed map, or declared-order struct, and records it at the call site. An enum, a class and an unconstrained generic have no recipe, and naming one at the call is a compile-time error.

A struct's fields arrive as `FieldRecipe`s: name, recipe, and a `FieldDefault` saying what an *omitted* field means, either required or a literal default baked in as JSON text. That is how a decode fills a defaulted field without re-entering the program.

At dispatch, both backends marshal the arguments, look up the module's `typed_dispatch`, and call it threaded the `&TypeRecipe`:

```rust
fn build_typed_dispatch(func: &str, host: &mut dyn Host, args: &[NativeValue], recipe: &TypeRecipe)
    -> Result<NativeOut, StdError>
```

The dispatch returns a `NativeOut` tree **already carrying its declared wrapper**: `NativeOut::Ok` or `Err` for a `Result` shape, `NativeOut::Some` or `None` for `Option`, and a plain value tree for `Plain`. A `Plain` door signals an unrecoverable failure with `Err(StdError)`, a runtime abort, while a recoverable door leaves the `Err` channel alone and returns its `Err` arm *inside* the `NativeOut`.

Both backends materialize that one tree with no per-function wrapping logic, so they agree by construction. `json.parse::<T>` and `try_parse::<T>` are registered exactly this way, and nothing about them is special-cased in the checker or either backend.

### Call-site-typed **methods**

An extern *type* gets the same surface through `ExtType::typed_methods` and `ExtType::typed_dispatch`, as in `resp.json::<User>()`. The rules are the module ones verbatim: a separate name space (a name may appear in both `methods` and `typed_methods`), a required `RetTy::TypeArg` return, and the same recipe contract. The dispatch also takes the receiver:

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

A turbofish method call reaches the checker as **either** `Expr::TypedModuleCall` (a bare-identifier receiver with one type argument, `r.json::<T>()`) or `Expr::TypedMethodCall` (anything else, `get(u)?.json::<T>()`). The split is purely syntactic: both spellings mean the same thing and both lower to `Op::TypedMethodCall`. What distinguishes a native typed call from an ordinary **erased** generic-method instantiation is whether the checker found the name in the receiver type's `typed_methods` table and recorded a recipe for that span.

## Directives that generate code (`ExtDirective::expand`)

An extension can register **`@`-directives**: `Extension::directives()` returns `ExtDirective` entries, which add a name to the decorator name-space. Resolution runs after the built-in directives and after the tier name-space, so an extension can never shadow either. Each entry declares where it may attach (`sites`), what arguments it takes (`max_args`, `named_keys`), and the prose the editor shows on hover and in completion.

A directive with an `expand` hook **generates the members of the declaration it decorates**. The hook receives the invocation and that declaration (`DirectiveCtx`: `args`, `named`, `target`, `site`, `source_dir`, `fields`) and returns Noeta source:

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

**A directive runs at compile time.** `@` is the language's codegen half and `#[…]` is the runtime-readable half (see [Attributes and Reflection](Attributes-and-Reflection)), so `attributes_of::<T>()` does not see a directive. An extension that wants runtime-visible metadata declares an attribute, and one that wants to consume a resource *dynamically* returns an invocable value.

**Placement and the argument contract are checked before the hook runs**, so a hook need not defend against a directive that sat somewhere it does not belong, or was called with arguments it never declared. Reading the filesystem is authorized by the package's `[trust]` grant; beyond that, a hook must be a pure function of its `DirectiveCtx` and the files it reports.

Failures are reported as **E0062**, blamed on the directive rather than on a generated line, with the position inside the generated source carried in the message.

### What the hook sees: `ctx.fields` and `ctx.target`

A hook generates from the declaration's *shape* as well as its name. `DirectiveCtx::fields` is the decorated declaration's members as `(name, declared type spelling)` pairs, in declaration order:

| Site | What `fields` holds |
|---|---|
| `struct`, `class` | its fields |
| `enum` | its **variants**, each with its payload spelling as declared: `"(index: int)"` for a named payload, `"(T)"` for a positional one, and the empty string for a variant that carries none |
| `Function`, `Method`, `Trait` | empty, since none of them declares typed members |

A validation directive can therefore emit one check per field, a persistence directive one column accessor per field, and a serializer the round-trip pair:

```rust
fn expand_columns(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    let mut source = String::new();
    for (name, ty) in &ctx.fields {
        source.push_str(&format!("fn {name}_column(): string {{ return \"{name}:{ty}\"; }}\n"));
    }
    Ok(Expansion { source, reads: Vec::new() })
}
```

A spelling is the **declared surface** one, at full fidelity: `List<int>` arrives as `"List<int>"` and never as `"List"`, and `?User` as `"?User"` and never as `"Option<User>"`, because a hook writes source and source is written in the surface language. An unannotated member reports `dyn`. The one adjustment is that a namespace-qualified identity renders as its short name (`std.id.Uuid` becomes `Uuid`), since generated code spelling the qualified identity would name something the consumer's file cannot resolve.

That derivation is the one the checker hands `ExtDerive::validate`, so a recipe and an expansion hook in one extension always see the same declaration the same way. A hook sees the decorated declaration and nothing of the surrounding program, and editing a field re-runs the expansion exactly as editing the directive's arguments does.

**`ctx.target` is a bare identifier.** The decorated declaration's name arrives unqualified even when its file is a package module carrying a qualified module path, because that is the only spelling in scope where the generated members land. A hook can put it straight into a constructor's return type or a struct literal (`fn new(api: Api): {target} { return {target} { … } }`).

### The generated source

**The output is Noeta source.** It goes through the real grammar, so generated code earns the same diagnostics as hand-written code and the result stays inspectable. What you may emit follows from where the directive attached: members of the decorated declaration, exactly as `@derive` synthesizes methods onto a type. `sites` already answers the question of output scope.

**Each expansion becomes a real source file**, registered in the program's source map under a name that says what caused it (`PetStore ⟨@openapi "petstore.yaml"⟩`), so generated members have true spans and a fault inside a generated method points at that method. Those sources are what [`noeta expand`](The-CLI#noeta-expand) prints, so a hook can be debugged against its real output, and that output diffed in CI, without provoking an error first.

### Declare every file the hook reads, on the error path too

`Expansion::reads` (and `ExpansionError::reads`) is the hook's incrementality contract. The compiler cannot discover these by parsing, and it does not simply hand you the file named in your arguments, because a spec routinely pulls in others (an OpenAPI `$ref` into a sibling document) and only the hook knows which.

Report every file opened, *including ones that turned out to be missing*, since their appearing later is a change too, and *including when the hook then fails*: the reads survive the `Err`, so a spec that is missing today re-runs the expansion the moment it is written. A hook that under-reports will serve stale members until something unrelated invalidates it. Under `--watch` (`test`, `run`, `serve`), the reported reads are watched alongside the `.noe` sources, so editing or creating a spec re-runs the generation, and the editor's incremental engine treats a change to one as a full re-check.

## Derive recipes (`ExtDerive`)

An extension can register **derive recipes**. `Extension::derives()` returns `ExtDerive { name, methods, validate }` entries, and `@derive(<Name>)` on a type synthesizes each declared method as a forward into the extension's registered module function, `fn <name>(a1: dyn, …): dyn { return <handler>(self, a1, …) }`, resolved like an expression tier's native handler and needing no user import.

The handler does its real work natively, typically by reflecting over the value. The optional `validate` hook rejects unsuitable type shapes at check time (E0050), reading the deriving type's name and its `(field name, declared type spelling)` pairs, the same shape, from the same derivation, that an expanding directive gets as [`DirectiveCtx::fields`](#what-the-hook-sees-ctxfields-and-ctxtarget).

std's own `Inspect` (`inspect()` becomes `json.stringify(self)`) is the reference example. Names resolve after built-in traits and the program's user traits, so a recipe can never shadow either.

## See also

- [Writing a Native Package](Writing-Native-Packages) — the author-facing walkthrough: entry crate, manifest, composition, building, testing, publishing.
- [Extension Compatibility](Extension-Compatibility) — the API contract for native package authors: the stable surface, what is not stable, and the pre-1.0 policy.
- [Standard library reference](Std) — the modules registered through this seam.
- [Concurrency Internals](Concurrency-Internals) — the `Host` capability's role in the deterministic/real split.
