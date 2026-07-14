# `packages/` — first-party, non-default Noeta packages

This directory holds **first-party but non-default** packages: capabilities the Noeta
project maintains, but that do **not** ship in the `std` stdlib. They are resolved through
the package manager (path/registry dependencies + `noeta.lock`) exactly as a third-party
package would be — this is where the registry / composed-toolchain / `[trust]` path is
dogfooded end to end.

The eventual endpoint is fully **out-of-tree** (standalone git repos + hosted registry);
these live in-tree for now as the intermediate first-party-non-default state.

## The `para` namespace

Both packages here sit under the top-level namespace **`para`** ("alongside", Greek *παρά*):

| Package        | Kind         | Modules                                   |
| -------------- | ------------ | ----------------------------------------- |
| `para-html`    | Noeta source | `para.html` (liveview)                    |
| `para-p2p`     | native (Rust)| `para.p2p`, `para.crdt`, `para.synced`    |

### How two package kinds share one `para` root

Noeta resolves a `use` path two ways, and `para.*` uses both without conflict:

- **Native modules** (`para.p2p/crdt/synced`) are registry-resolved. A native `Extension`
  whose `root()` returns `"para"` claims the root; each `ExtModule` resolves by its
  root-qualified identity (`para.p2p`, …). Multiple extensions may share a root (the `std.*`
  units already do), so one `ParaP2pExtension` carries all three modules.

- **Source modules** (`para.html`) are loaded-module resolved. The linker re-roots a
  dependency's declared namespace by its **leading segment** (`reroot_path`,
  `crates/noeta-loader/src/lib.rs`): the `para-html` package's module declares
  `namespace <pkgroot>.html`, the consumer keys the dependency `para`, and the leading
  segment is rewritten to yield `para.html`. Consumers write `use para.html.<name>`.

These never collide even though `is_extension_root("para")` is true: the native-module
gate (`is_native_module` in `crates/noeta-compiler/src/lib.rs`) requires **both**
`is_extension_root(path[0])` *and* an exact `find_module(path)` hit. `para.html` is not a
registered native module, so it falls through to source resolution; `para.p2p` is, so it
takes the native path. Root-existence alone never hijacks a source module.

## Going out-of-tree (follow-on F3)

A first-party package leaves the monorepo by becoming its **own standalone git repo** with
`noeta.toml` at the root (the registry maps `company/package@version` → whole-repo git
coordinates — url + tag + pinned SHA — with no subdirectory support), published to an index
that consumers resolve against. The pure-source `para-html` needs nothing beyond this; the
whole round-trip is verified against a **local** git repo + a `LocalIndex`:

```sh
# 1. Lift the package into a standalone repo (its noeta.toml is already at the root) and tag it.
cp para-html/*.noe para-html/noeta.toml <REPO>/ && cd <REPO>
git init -q && git add -A && git commit -qm 'para/html v0.1.0' && git tag v0.1.0

# 2. Publish to a local index by the repo's file:// URL (pins the tag → commit SHA + a docs blob).
NOETA_REGISTRY_DIR=<INDEX> noeta publish --git "file://<REPO>" --tag v0.1.0

# 3. A fresh consumer ANYWHERE resolves it purely from the index — a registry dep, not a path:
#    [dependencies] para = { version = "^0.1", package = "noeta/para" }
NOETA_REGISTRY_DIR=<INDEX> noeta run main.noe   # → clones by pinned SHA, links use para.html.*
```

`noeta.lock` then pins `source = "git"`, the `url`/`tag`/`sha`, the content `hash`, and the
package's language `edition` — so the build reproduces the exact source and edition offline.
The `@html` expression tier travels with the package (importing `render` brings its
`@tier(html, …, expr: Html)` declaration into scope).

**Native packages (`para-p2p`) reference the toolchain's Rust crates by git, not crates.io.**
A native entry crate compiled by the composed toolchain depends on `noeta-native` /
`noeta-stdlib` / `noeta-crdt` / `noeta-reactive` — these are *toolchain* crates, versioned
with the language. A standalone `para-p2p` repo references them the same way the composer
already pulls its base (`ToolchainSource::GitTag`): a **git dependency on the lang repo at a
version tag**. They are never published to a foreign registry — Noeta packages resolve
through the Noeta registry, and their native crates resolve toolchain crates from the Noeta
repo. This is why `para-p2p`'s split is a larger step than `para-html`'s (it must rewrite its
native crate's dependency edges from monorepo workspace paths to git-tag deps), tracked as its
own slice.

The monorepo keeps the in-tree copies here as the committed source of truth (so a fresh
checkout stays self-contained and portable — a `file://` URL is machine-specific); the true
departure — removing the in-tree copy and repointing consumers at the registry — lands with
the **hosted** registry (a real, portable remote URL), follow-on F4.
