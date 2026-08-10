# Writing a Native Package

This is the author-facing walkthrough for shipping a package that contains native (Rust) code: the entry crate, the manifest wiring, how the composed toolchain is built, and what to watch for when your package lives in its own repository. The concepts behind the seam — the registry, the `Host` capability, the dispatch contracts — are on [Native Extensions](Native-Extensions); the compatibility contract you are writing against is [Extension Compatibility](Extension-Compatibility).

## The entry crate and the manifest

A dependency package ships native code by naming an **entry crate** in its manifest:

```toml
# the package's noeta.toml
[package]
name = "acme/imgfx"
version = "1.0.0"
native = "native"        # relative dir containing the entry crate's Cargo.toml
```

The entry crate is an ordinary Rust library that depends on `noeta-ext-abi` and exports its extension units as a slice — one crate, any number of units (core's own `std` is five units in one crate):

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

## What composition does

The consumer's app never configures any of this: when `noeta run`/`check`/`build`/… sees a dependency graph with native crates, it generates a ~20-line shim crate (depend on the `noeta-cli` *library* + each entry crate; `main` passes the aggregated `NOETA_EXTENSIONS` units into `run_cli`), builds it with cargo, caches the binary content-addressed (keyed on the toolchain's build identity + each entry crate's tree), and **exec-delegates**. The composed binary *is* the app's toolchain: the checker sees the extension's signatures (a wrong-typed argument to a native function is a static error), the LSP its completions, the CLI its commands. A pure-Noeta app never touches any of this. The one requirement composition adds: a consumer of a native-dep package needs a **Rust toolchain** on PATH (the diagnostic says so by name when it's missing) — the composed build then runs once per dependency-set change, and every later invocation is a single exec.

The toolchain's own source resolves in order: `NOETA_TOOLCHAIN_SRC` (a checkout override, hermetic setups) → the workspace the running binary was built in (path deps — the development norm) → a git dependency pinned to the running binary's version tag (`noeta-cli = { git = …, tag = "vX.Y.Z" }` — cargo's own git cache does the fetching).

## Versioning

The consumed crates (`noeta-ext-abi`, `noeta-reactive-abi`, `noeta-cli` as a lib, `noeta-stdlib`) are versioned and git-tagged together, and a composed shim resolves every toolchain crate — whatever tag your package pins — onto the consumer binary's own toolchain source. The full compatibility statement — what is stable, what is not, the pre-1.0 policy, and how versions resolve at consume time — is [Extension Compatibility](Extension-Compatibility); read it before publishing.

## Out-of-tree packages need exactly one copy of the toolchain

A standalone package repo can't path-depend the noeta monorepo, so its entry crate names the contract crate from **crates.io**:

```toml
# a standalone package's native/Cargo.toml
[dependencies]
noeta-ext-abi = "0.6"
```

`noeta-ext-abi` and `noeta-reactive-abi` are the only toolchain crates published there — they are the whole stable surface (see [Extension Compatibility](Extension-Compatibility)). A range rather than an exact version, because a *patch* toolchain release does not change the contract and should not cost you a manifest edit.

Reaching past the contract means git-depending instead, since the internal crates are unpublished:

```toml
# only if you need something the contract does not expose
noeta-loader = { git = "https://github.com/…/noeta", tag = "vX" }
```

The subtlety is a Rust one: a type's identity includes *which compiled copy of the crate* it came from, so if `noeta-ext-abi` is compiled twice — once for the shim, once for the git entry crate — its `Extension` trait exists **twice** as two unrelated types, and the shim's `units.extend_from_slice(ext0::NOETA_EXTENSIONS)` no longer type-checks (a `dyn Extension` from one copy doesn't satisfy the other). The whole graph must resolve `noeta-ext-abi` to **one** source. Two cases:

- **The consumer runs a released (git-tag) toolchain.** The shim pins the toolchain by the binary's own release tag, and the composer injects a **`[patch]`** on the canonical repo URL that rewrites every `crates/*` member to a cached checkout of that same tag — so the package's pin collapses onto the consumer's toolchain *whatever tag the package declares*. A package pinned at an older release still composes under a newer binary; the pin governs only the package's own repository CI.
- **The consumer runs a workspace (local-path) toolchain** — the development norm, and while iterating on the toolchain itself. Now the shim's `noeta-ext-abi` is a *path* and the package's is *git* — two sources. The composer closes this by injecting a **`[patch]`** into the shim that rewrites every `crates/*` member of the noeta repo to the consumer's exact path, so the package's git-deps (and their transitive `workspace = true` deps) all collapse onto the one local copy. The patch key is the toolchain's own `repository`, overridable with **`NOETA_TOOLCHAIN_REPO`** for a fork, a private mirror, or a local `file://` clone (it must equal the URL the package's `Cargo.toml` declares). Cargo *does* fetch the git source before applying the patch, so the toolchain repo must be reachable — fine for a public repo.

Both cases above describe a **git** dependency, and the composer emits a matching `[patch."<repo url>"]`. A **crates.io** dependency needs its own table, so the composer emits `[patch.crates-io]` alongside it with the same redirects — without that, `noeta-ext-abi = "0.6"` would resolve the real published crate and you would be back to two compiled copies and a `dyn Extension` that does not match. Whichever form you write, the graph collapses onto the consumer's one toolchain.

So an out-of-tree package (the `para` family, and any third-party one) depends on the contract crate by version and lets composition redirect it; a package that git-depends an internal crate is served by the same machinery. In-tree packages keep a path dep and never touch any of this.

## Where heavy dependencies belong

An extension whose implementation needs a heavy native tree should either put the effectful part **behind the `Host` capability seam** (the runtime side, where `noeta build --native` can gate it behind a ring feature — how `std.http`'s reqwest tree stays out of non-http binaries) or accept that its whole crate is the include/exclude unit. Unconditional heavy deps in an always-linked crate cannot be dead-code-eliminated per-program; core's own `crypto`/`id` (~65 KB of sha2/bcrypt/uuid, unconditional in `noeta-stdlib`) are the recorded example of accepting that cost — the extension-unit split makes gating them mechanical if a size budget ever demands it.

## Composition for a shipped artifact — the lean runner

The composed toolchain above is the *development* binary. A **shipped** artifact is composed differently: when `noeta build` sees native runtime dependencies, it composes a **lean base** carrying your extension's runtime units but **none of the toolchain** — no fmt, no LSP, no DAP, no formatter parsers. The base's form matches the emit:

- **`--exe`** composes a **runner binary** — the same aggregation of `NOETA_EXTENSIONS` units, but the base is the lean `noeta-runner` (not `noeta-cli`), and `main` calls `run_stapled_with_extensions` instead of `run_cli`; the program's bundle staples onto it.
- **`--native`** composes an **AOT-runtime staticlib** — a `staticlib` shim on `noeta-aot-runtime` (its own C `main` off, your program's stdlib rings forwarded) whose `main` installs the units via `run_embedded_with_extensions`; the `cc` link combines it with the program's AOT machine-code object. So a native-dependency app compiles to a self-contained native binary that still resolves your native modules.

Each composition carries your extension's **runtime** capabilities (modules, types, tier handlers) only. The compositions cache separately by kind; a pure-Noeta app skips composition and uses the stock lean runner / `libnoeta_aot.a`.

## Shipping dev capabilities — gate them behind a feature

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

## Building and testing

Day to day, the package repo is an ordinary Rust workspace: `cargo build`/`cargo test` against the toolchain tag your `Cargo.toml` pins. Two additions are worth setting up early:

- **A conformance corpus of your own.** The toolchain's executable-spec runner ships as the `noeta-conformance` crate; take it (plus `noeta-stdlib`, to install the `std` registry) as **dev-dependencies**, keep a corpus of `.noe` fixtures with `// expect:` headers, install your extension into the process registry (`noeta_stdlib::registry::install_with_extras`), and assert every fixture with `run_corpus` — then assert the two backends agree byte-for-byte with `run_differential`. This is the recommended tripwire for toolchain-release breakage; [Extension Compatibility](Extension-Compatibility#pre-10-policy) shows the exact dependency block and explains why.
- **CI against the current release, not just your pin.** Composition builds your crate against the *consumer's* toolchain, so your CI should also install the current released `noeta` and run an example program through it (composing the toolchain builds your native crate in the process). See [Extension Compatibility](Extension-Compatibility#one-toolchain-wins-how-versions-resolve-at-consume-time) for why your pin does not protect you.

While iterating locally you can point a consumer project's manifest at your package by path, and — when you are also modifying the toolchain — run a workspace toolchain with `NOETA_TOOLCHAIN_REPO` set if your package's git-deps name a fork or mirror.

## Publishing

A native package publishes like any other package: `noeta publish` to the hosted registry at [registry.noeta.dev](https://registry.noeta.dev) (keyless Sigstore-signed and transparency-logged), after which consumers add it as an ordinary registry dependency. Three native-specific considerations:

- **Declare a `toolchain` floor** in `noeta.toml` (`toolchain = ">=0.2"`) — the oldest release you actually test against — so an old consumer gets a clear "run `noeta upgrade`" instead of a compile error deep inside a native build. See [Manifest](Manifest).
- **Consumers need a Rust toolchain** on PATH to compose your package; say so in your README.
- **Trust:** a consumer must authorize a native dependency in `[trust]` before it is installed and composed.

See [Package Registries](Package-Registries) and [Package Provenance](Package-Provenance) for scopes, claiming, and signing.

## See also

- [Native Extensions](Native-Extensions) — the concepts: the registry, the dispatch seams, extern types/classes/traits, the `Host` capability.
- [Extension Compatibility](Extension-Compatibility) — the API contract: the stable surface, what is not stable, and the pre-1.0 policy.
- [Manifest](Manifest) — the `native` and `toolchain` keys.
- [Using Packages](Using-Packages) — the consumer's view, including `[trust]`.
