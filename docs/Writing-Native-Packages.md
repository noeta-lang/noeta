# Writing a Native Package

This is the author-facing walkthrough for shipping a package that contains native (Rust) code: the entry crate, the manifest wiring, how the composed toolchain is built, and what to watch for when your package lives in its own repository. The concepts behind the seam, the registry and the `Host` capability and the dispatch contracts, are on [Native Extensions](Native-Extensions). The compatibility contract you are writing against is [Extension Compatibility](Extension-Compatibility).

## The entry crate and the manifest

A dependency package ships native code by naming an **entry crate** in its manifest:

```toml
# the package's noeta.toml
[package]
name = "acme/imgfx"
version = "1.0.0"
native = "native"        # relative dir containing the entry crate's Cargo.toml
```

The entry crate is an ordinary Rust library that depends on `noeta-ext-abi` and exports its extension units as a slice. One crate holds any number of units, and core's own `std` is five units in one crate:

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

Registration literals should spell only what they use and default the rest, as in `ExtModule { name, functions, dispatch, ..ExtModule::DEFAULTS }` (same for `ExtType`, `ExtFn`, `ExtCommand`), so a future optional field is additive rather than breaking.

## What composition does

When `noeta run`, `check`, `build` or any other command sees a dependency graph with native crates, it generates a shim crate of about twenty lines. The shim depends on the `noeta-cli` *library* and on each entry crate, and its `main` passes the aggregated `NOETA_EXTENSIONS` units into `run_cli`. The CLI builds that shim with cargo, caches the binary content-addressed (keyed on the toolchain's build identity and each entry crate's tree), and **exec-delegates** to it.

The composed binary *is* the app's toolchain: the checker sees the extension's signatures, so a wrong-typed argument to a native function is a static error, the LSP sees its completions, and the CLI sees its commands. The consumer's app configures none of this, and a pure-Noeta app never touches it.

Composition adds one requirement. A consumer of a native-dep package needs a **Rust toolchain** on PATH, and the diagnostic says so by name when it is missing. The composed build then runs once per dependency-set change, and every later invocation is a single exec.

The toolchain's own source resolves in this order:

1. `NOETA_TOOLCHAIN_SRC`, a checkout override for hermetic setups.
2. The workspace the running binary was built in (path deps, the development norm).
3. A git dependency pinned to the running binary's version tag (`noeta-cli = { git = …, tag = "vX.Y.Z" }`), where cargo's own git cache does the fetching.

## Versioning

The consumed crates (`noeta-ext-abi`, `noeta-reactive-abi`, `noeta-cli` as a lib, `noeta-stdlib`) are versioned and git-tagged together, and a composed shim resolves every toolchain crate, whatever tag your package pins, onto the consumer binary's own toolchain source. [Extension Compatibility](Extension-Compatibility) is the full statement of what is stable, what is not, the pre-1.0 policy, and how versions resolve at consume time. Read it before publishing.

## Out-of-tree packages need exactly one copy of the toolchain

A standalone package repo cannot path-depend the noeta monorepo, so its entry crate names the contract crate from **crates.io**:

```toml ignore
# a standalone package's native/Cargo.toml
[dependencies]
noeta-ext-abi = "0.8"
```

`noeta-ext-abi` and `noeta-reactive-abi` are the only toolchain crates published there, and they are the whole stable surface (see [Extension Compatibility](Extension-Compatibility)). Name a range rather than an exact version, because a *patch* toolchain release does not change the contract and should not cost you a manifest edit.

Reaching past the contract means git-depending instead, since the internal crates are unpublished:

```toml ignore
# only if you need something the contract does not expose
noeta-loader = { git = "https://github.com/…/noeta", tag = "vX" }
```

The subtlety is a Rust one. A type's identity includes *which compiled copy of the crate* it came from, so if `noeta-ext-abi` is compiled twice, once for the shim and once for the git entry crate, its `Extension` trait exists **twice** as two unrelated types. The shim's `units.extend_from_slice(ext0::NOETA_EXTENSIONS)` then stops type-checking, because a `dyn Extension` from one copy does not satisfy the other. The whole graph must resolve `noeta-ext-abi` to **one** source. Two cases arise:

- **The consumer runs a released (git-tag) toolchain.** The shim pins the toolchain by the binary's own release tag, and the composer injects a **`[patch]`** on the canonical repo URL that rewrites every `crates/*` member to a cached checkout of that same tag. The package's pin collapses onto the consumer's toolchain *whatever tag the package declares*, so a package pinned at an older release still composes under a newer binary, and the pin governs only the package's own repository CI.
- **The consumer runs a workspace (local-path) toolchain**, which is the development norm and the case while iterating on the toolchain itself. Here the shim's `noeta-ext-abi` is a *path* and the package's is *git*, which is two sources. The composer closes this by injecting a **`[patch]`** into the shim that rewrites every `crates/*` member of the noeta repo to the consumer's exact path, so the package's git-deps and their transitive `workspace = true` deps all collapse onto the one local copy. The patch key is the toolchain's own `repository`, overridable with **`NOETA_TOOLCHAIN_REPO`** for a fork, a private mirror, or a local `file://` clone, and it must equal the URL the package's `Cargo.toml` declares. Cargo fetches the git source before applying the patch, so the toolchain repo must be reachable, which is fine for a public repo.

Both cases describe a **git** dependency, and the composer emits a matching `[patch."<repo url>"]`. A **crates.io** dependency needs its own table, so the composer emits `[patch.crates-io]` alongside it with the same redirects. Without that, a published-crate requirement would resolve the real published crate, leaving two compiled copies and a `dyn Extension` that does not match. Whichever form you write, the graph collapses onto the consumer's one toolchain.

So an out-of-tree package, the `para` family and any third-party one alike, depends on the contract crate by version and lets composition redirect it, and a package that git-depends an internal crate is served by the same machinery. In-tree packages keep a path dep and never touch any of this.

## Where heavy dependencies belong

An extension whose implementation needs a heavy native tree has two options. Put the effectful part **behind the `Host` capability seam**, the runtime side, where `noeta build --native` can gate it behind a ring feature, which is how `std.http`'s reqwest tree stays out of non-http binaries. Otherwise, accept that its whole crate is the include and exclude unit.

Unconditional heavy deps in an always-linked crate cannot be dead-code-eliminated per program. Core's own `crypto` and `id` carry about 65 KB of sha2, bcrypt and uuid unconditionally in `noeta-stdlib`, and the extension-unit split makes gating them mechanical if a size budget demands it.

## Composition for a shipped artifact: the lean runner

The composed toolchain above is the *development* binary. A **shipped** artifact is composed differently: when `noeta build` sees native runtime dependencies, it composes a **lean base** carrying your extension's runtime units and none of the toolchain, so no fmt, no LSP, no DAP, and no formatter parsers. The base's form matches the emit:

- **`--exe`** composes a **runner binary**, the same aggregation of `NOETA_EXTENSIONS` units, on the lean `noeta-runner` base rather than `noeta-cli`, whose `main` calls `run_stapled_with_extensions` instead of `run_cli`. The program's bundle staples onto it.
- **`--native`** composes an **AOT-runtime staticlib**, a `staticlib` shim on `noeta-aot-runtime` (its own C `main` off, your program's stdlib rings forwarded) whose `main` installs the units via `run_embedded_with_extensions`. The `cc` link combines it with the program's AOT machine-code object, so a native-dependency app compiles to a self-contained native binary that still resolves your native modules.

Each composition carries your extension's **runtime** capabilities only, meaning modules, types and tier handlers. The compositions cache separately by kind, and a pure-Noeta app skips composition and uses the stock lean runner or `libnoeta_aot.a`.

## Shipping dev capabilities: gate them behind a feature

An `Extension`'s capabilities split by *kind*. `modules`, `types`, `tiers` and `commands` are **runtime**, needed to run the program. `body_formatters`, the tier-body formatter `noeta fmt` uses, is **dev-only**: it and its parser (a CSS or HTML reformatter is a *parser*, which is attack surface) must never ride into a production binary.

A single crate that ships both a runtime tier handler and its formatter is a **mixed package**. Keep the formatter out of shipped artifacts by gating it, and any heavy formatting dependency, behind a Cargo feature:

```toml ignore
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

Because the feature is **off by default**, every shipped base (the composed runner and the composed AOT runtime, both built with default features) compiles neither the formatter nor `malva`. The shipped artifact is lean automatically, with no per-dependency configuration by the app author.

The composed **dev toolchain** turns this feature **on**. Name it `fmt`, the conventional dev-capability feature, and the toolchain composition enables it automatically, so `noeta fmt` reflows your tier's bodies. Only a feature your crate actually declares is enabled, so the convention is opt-in and a pure-runtime crate that declares no `fmt` feature is untouched. The same shape works for any dev-only capability whose implementation drags in a parser or other heavy tree.

## Building and testing

Day to day, the package repo is an ordinary Rust workspace: `cargo build` and `cargo test` against the toolchain tag your `Cargo.toml` pins. Two additions are worth setting up early:

- **A conformance corpus of your own.** The toolchain's executable-spec runner ships as the `noeta-conformance` crate. Take it, plus `noeta-stdlib` to install the `std` registry, as **dev-dependencies**, keep a corpus of `.noe` fixtures with `// expect:` headers, install your extension into the process registry (`noeta_stdlib::registry::install_with_extras`), and assert every fixture with `run_corpus`, which checks each header against both engines. Then assert the two backends agree byte-for-byte with `run_differential`. This is the recommended tripwire for toolchain-release breakage; [Extension Compatibility](Extension-Compatibility#pre-10-policy) shows the exact dependency block and explains why.
- **CI against the current release, not just your pin.** Composition builds your crate against the *consumer's* toolchain, so your CI should also install the current released `noeta` and run an example program through it, which composes the toolchain and builds your native crate in the process. See [Extension Compatibility](Extension-Compatibility#one-toolchain-wins-how-versions-resolve-at-consume-time) for why your pin does not protect you.

While iterating locally you can point a consumer project's manifest at your package by path. When you are also modifying the toolchain, run a workspace toolchain with `NOETA_TOOLCHAIN_REPO` set if your package's git-deps name a fork or mirror.

## Publishing

A native package publishes like any other package. `noeta publish` sends it to the hosted registry at [registry.noeta.dev](https://registry.noeta.dev), keyless Sigstore-signed and transparency-logged, after which consumers add it as an ordinary registry dependency. Three considerations are native-specific:

- **Declare a `toolchain` floor** in `noeta.toml` (`toolchain = ">=0.2"`), the oldest release you actually test against, so an old consumer gets a clear "run `noeta upgrade`" instead of a compile error deep inside a native build. See [Manifest](Manifest).
- **Consumers need a Rust toolchain** on PATH to compose your package. Say so in your README.
- **Trust.** A consumer must authorize a native dependency in `[trust]` before it is installed and composed.

See [Package Registries](Package-Registries) and [Package Provenance](Package-Provenance) for scopes, claiming, and signing.

## See also

- [Native Extensions](Native-Extensions) — the concepts: the registry, the dispatch seams, extern types/classes/traits, the `Host` capability.
- [Extension Compatibility](Extension-Compatibility) — the API contract: the stable surface, what is not stable, and the pre-1.0 policy.
- [Manifest](Manifest) — the `native` and `toolchain` keys.
- [Using Packages](Using-Packages) — the consumer's view, including `[trust]`.
