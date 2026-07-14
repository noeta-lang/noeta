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
