# Extension Compatibility

This page is the compatibility statement for **native package authors**—the answer to "what may my
Rust code depend on, what happens to that dependency when a consumer builds against a different
toolchain release, and how do I find out I broke before my users do?" The mechanics of *writing* an
extension live in [Native Extensions](Native-Extensions); this page is only about the contract.

The short version:

1. **`noeta-ext-abi` is the API you write against.** It (plus `noeta-reactive-abi`, if you integrate
   with the reactive engine) is the surface with a compatibility promise.
2. **Everything else in the toolchain is internal.** You *can* git-depend on internal crates—some
   first-party packages do—but they carry no stability promise at all.
3. **At consume time, the consumer's toolchain wins.** Your pinned toolchain tag governs only your
   own repository's CI; toolchain composition rebuilds your crate from source against the consumer's
   copy of every toolchain crate.
4. **Pre-1.0, a minor release may break you.** Breaks are listed in release notes, and the
   conformance harness is the recommended tripwire.

## The stable surface: `noeta-ext-abi`

A package's native entry crate depends on **`noeta-ext-abi`** and nothing else of the toolchain.
The crate is deliberately lean—its only dependencies are `compact_str`/`equivalent`/`hashbrown`,
none of core's batteries—and it contains the whole registration and dispatch contract:

- **The `Extension` trait**—the unit of registration. Two methods are required (`name`, `modules`);
  everything else is defaulted, so a modules-only extension stays source-compatible as declaration
  surfaces are added: `root` (namespace root, defaults to the name), `types`, `enums`, `classes`,
  `structs`, `traits`, `commands`, `tiers`, `attributes`, `directives`, `derives`,
  `body_formatters`, and `capabilities`.
- **The `NOETA_EXTENSIONS` symbol convention**—the entry crate exports
  `pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)]`, the slice the composed toolchain
  aggregates.
- **The registration vocabulary**—`ExtModule`/`ExtFn` with `SigType`/`RetTy` signatures (including
  `SigType::Optional` trailing-optionals, `SigType::Fn` callables, `SigType::Generic` + `Var` type
  variables), and the call-site-typed surface (`typed_functions`/`typed_methods`, `TypeArgWrap`,
  `TypeRecipe`).
- **The marshalling layer**—`NativeValue` in, `NativeOut` out (including the bulk
  `NativeOut::Scalars(ScalarVec)` form and the class/struct twins `Instance`/`Struct`), plus
  `StdError`/`ErrorKind` and the canonical error builders (`arity_error`, `type_error`, …) that keep
  both backends' diagnostics identical.
- **The type-declaration surfaces**—`ExtType` + the `ExternValue`/`ExternBox` value contract and
  `MapKey`; `ExtClass`/`ExtStruct` with `ExtField`; `ExtEnum` with `ExtVariant`; `ExtTrait` with
  `ExtTraitMethod`, associated types, and the structural `PackedConstraint` used by kernel bundles.
- **CLI commands**—`ExtCommand { name, about, args, run }` with typed `ArgSpec`s, driven through a
  narrow `CommandCtx`.
- **Compile-time codegen**—`ExtDirective` (the `expand` hook) and `ExtDerive` recipes.
- **The higher-order seam**—`NativeCtx` with opaque `Slot`s (`call`/`spawn_io`/`timer`/`poll`/
  `drive`, scheduler advancement), per-run `ExtState`, the retained arena (`Retained`), the raw
  packed-buffer capabilities (`PackedView`, `with_packed`/`with_packed_mut`/`make_packed_like`),
  and the `TaskContext`/`FutureTracing`/`HotReload` sub-traits.
- **The capability broker**—`ExtCapability` on the provider side, `capability::<dyn Trait>(ctx)` on
  the consumer side, for cross-extension collaboration without either side naming the other's
  types.
- **The `Host` capability seam**—the trait an effectful dispatch receives, its per-capability traits
  (filesystem, clock, RNG, env/args, console, OS, entropy, ids, network, telemetry), the
  `real_p2p()` policy, and the async `ExternIo`/`Executor`/`RealBody` seam.

Two conventions are part of the promise. First, **additive evolution is done with `..DEFAULTS`**,
not `#[non_exhaustive]`: registration literals spell only what they use
(`ExtModule { name, functions, dispatch, ..ExtModule::DEFAULTS }`), so a new optional field lands
without breaking existing registrations. Second, the crate carries an **`ABI_VERSION`** constant;
today every extension is compiled from source against the exact toolchain (see composition below),
so an ABI change surfaces as an ordinary compile error and the constant is *recorded, not checked*—
it exists as the handshake a future dynamic-loading path would refuse a mismatch with.

A minimal entry crate, for orientation (see
[Writing a native package](Native-Extensions#writing-a-native-package) for the full walkthrough):

```rust
use noeta_ext_abi::registry::{ExtModule, Extension};

struct ImgfxExtension;
impl Extension for ImgfxExtension {
    fn name(&self) -> &'static str { "imgfx" }          // root defaults to name()
    fn modules(&self) -> &'static [ExtModule] { /* … */ }
}

/// The composition convention: the symbol the composed toolchain links.
pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&ImgfxExtension];
```

### `noeta-reactive-abi`, for reactive integrations

An extension whose values participate in the reactive graph—a synced signal, a live query—also
depends on **`noeta-reactive-abi`**, the contract crate between the reactive engine and a foreign
source node: the `ReactiveSource` trait (`create_source`/`read_source`/`wake`), obtained per run via
`noeta_ext_abi::capability::<dyn ReactiveSource>(ctx)`, and the inverse-direction
`ViewSourceExtract` capability. This is the model for future cross-extension contracts generally:
each is a small object-safe trait in its own `*-abi` crate, and those contract crates share
`noeta-ext-abi`'s stability promise. `noeta-ext-abi` itself never names any concrete capability
trait.

## What is explicitly not stable

**Everything that is not a contract crate.** Concretely:

- **`noeta-embed`**—the host-process embedding API (`Session`, the `Value` bridge, `Handle`s)—is
  **unstable by decision**: it is a 0.x surface that adapts to its consumers until a real engine
  integration has exercised it, and its own documentation says to expect breaking changes between
  minor versions.
- **The internal crates**—`noeta-loader`, `noeta-check`, `noeta-lexer`, `noeta-ast`,
  `noeta-stdlib`, `noeta-ir`, and the rest of the workspace. These are the toolchain's own
  organs. Nothing stops a package from git-depending on them, and some first-party packages
  currently do—`para/api`'s impl crate pulls `noeta-loader`/`noeta-lexer`/`noeta-ast`/
  `noeta-check`/`noeta-stdlib` to run Noeta code inside its directive expansion, and `para/p2p`
  uses `noeta-stdlib` as a dev-dependency so its conformance fixtures actually execute. That is
  allowed, and composition will patch those crates to the consumer's toolchain just like the ABI
  crate—but honestly: **they carry no stability promise**. Their APIs may change in any release,
  without a deprecation cycle and without being called out in release notes. A package that
  reaches past `noeta-ext-abi` accepts keeping pace with the toolchain by hand.

The rule of thumb: if the crate name does not end in `-abi`, treat every use of it as a fork-risk
you have chosen, not a contract the toolchain owes you.

## One toolchain wins: how versions resolve at consume time

A standalone package pins its toolchain crates by git tag—the release it is built and tested
against:

```toml
# the package's native/Cargo.toml
[dependencies]
noeta-ext-abi = { git = "https://github.com/noeta-lang/noeta", tag = "v0.2.0" }
```

That pin governs **only the package's own repository**: `cargo test` in your CI, your local builds.
When a *consumer* depends on your package, toolchain composition builds your crate again—from
source, inside the consumer's composed shim—and resolves every toolchain crate to **the consumer's
own toolchain version**, not your tag:

- With a **workspace (local-path) toolchain**, the composer injects a `[patch]` section keyed on
  the canonical toolchain repository URL that rewrites *every* `crates/*` member to the consumer's
  exact toolchain source (the key is this build's `repository`, overridable with
  `NOETA_TOOLCHAIN_REPO`—it must equal the URL your `Cargo.toml` declares). Your git pin is
  overridden wholesale.
- With a **released (git-tag) toolchain**, the shim's own dependencies use the running binary's
  version tag, and cargo unifies your pin onto the same source when the tags agree.

Either way the whole graph resolves to **one copy** of each toolchain crate—necessary for Rust type
identity (a second compiled copy of `noeta-ext-abi` would make your `dyn Extension` a different
type than the shim's)—and that copy is the consumer's. Three consequences worth internalizing:

- **The effective compatibility contract is the `noeta-ext-abi` API across versions.** Your package
  will be compiled against toolchain releases you have never seen. What protects you is source-level
  compatibility of the ABI crates, not your pin.
- **The manifest's `toolchain` key is your declared floor.** `toolchain = ">=0.2"` in `noeta.toml`
  tells the resolver the minimum `noeta` your package works with, enforced at resolve time with a
  clear "run `noeta upgrade`" message instead of a compile error deep inside a native build. It is a
  courtesy floor, not the contract itself—declare the oldest release you actually test against. See
  [Manifest](Manifest) for the exact requirement grammar (and the caret footgun: prefer `">=0.2"`
  over a bare `"0.2"`).
- **CI against the current release, not just your pin.** Since consumers track the toolchain's
  releases, your CI should install the current released `noeta` and run your package's example or
  test program through it (composing the toolchain builds your native crate in the process). The
  first-party `para` repositories share one pattern worth copying: the release to test against is a
  single CI variable set at the organization level, so bumping it on a new toolchain release
  re-proves every package at once without editing any workflow.

## Pre-1.0 policy

The consumed crates (`noeta-ext-abi`, `noeta-reactive-abi`, `noeta-cli` as the composed base,
`noeta-stdlib`) are versioned and git-tagged **together**, and compatibility is ordinary
source-level semver for a 0.x line:

- **A minor release may break extension code** during the alpha. Patch releases are additive.
- **Breaking ABI changes are listed in the release notes** of the release that ships them, with the
  mechanical fix alongside.
- **The conformance harness is the recommended tripwire.** The toolchain's executable-spec runner
  ships as the `noeta-conformance` crate; a package takes it as a dev-dependency, keeps a corpus of
  `.noe` fixtures with `// expect:` headers, and runs them with its own extension installed. This is
  exactly how `para/p2p` guards its surface: its test installs `std` plus `ParaP2pExtension` into
  the process registry (`noeta_stdlib::registry::install_with_extras`), asserts every fixture's
  expectation with `run_corpus`, and then asserts the two backends agree byte-for-byte with
  `run_differential`. When you bump your toolchain pin—or when CI's current-release run goes red—a
  conformance failure tells you *which fixture, which stage* drifted, before any consumer sees it.

```toml
# the package's native crate — test-only, does not ship
[dev-dependencies]
noeta-conformance = { git = "https://github.com/noeta-lang/noeta", tag = "v0.2.0" }
noeta-stdlib      = { git = "https://github.com/noeta-lang/noeta", tag = "v0.2.0" }
```

## See also

- [Native Extensions](Native-Extensions)—how to write an extension: the registry, types, classes,
  traits, directives, and the composed-toolchain build.
- [Manifest](Manifest)—the `native` and `toolchain` keys, and the dependency table.
- [Package Registries](Package-Registries) and [Package Provenance](Package-Provenance)—publishing,
  scopes, and signing.
