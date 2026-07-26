# Using Packages

This page is the practical walkthrough: install the toolchain, scaffold a project, add a
dependency from the hosted registry, grant it what it needs, and run. The reference pages behind
each step: [the `noeta.toml` Manifest](Manifest), [Package Registries](Package-Registries), and
[Package Provenance](Package-Provenance).

## 1 · Install `noeta`

One line on Linux or macOS (x86_64/aarch64 — the platforms with prebuilt binaries at alpha):

```sh
curl -fsSL https://noeta.dev/install | sh
```

Later releases are one `noeta upgrade` away. On macOS the binaries are not Apple-notarized: if
Gatekeeper blocks a browser-downloaded copy, clear the quarantine flag with
`xattr -d com.apple.quarantine <path-to>/noeta`. Details and the build-from-source path for other
platforms: [Getting Started](Getting-Started#1--install-the-toolchain).

## 2 · Scaffold a project

```console
$ noeta init hello
initialized Noeta package `local/hello` in hello
$ cd hello
```

You get a `noeta.toml` (the manifest), `src/main.noe`, and editor/agent scaffolding — all
documented at [The CLI](The-CLI#noeta-init). The project runs before you edit anything:

```console
$ noeta run src/main.noe
```

A program with no dependencies needs none of what follows — the whole standard library is built
in. Packages are for everything beyond it.

```noeta
fn greet(name: string): string {
    return "Hello, ${name}!"
}
echo greet("packages")
```

## 3 · Add a dependency

Dependencies live in the manifest's `[dependencies]` table. The first-party `para/*` packages —
`para/aether` (web framework), `para/db` (database drivers), `para/html` (LiveView), `para/api`,
`para/cli`, `para/aether_db`, `para/p2p` — are published on the hosted registry at
[registry.noeta.dev](https://registry.noeta.dev), so a version requirement is all it takes.
Several packages of the same scope bind under **one** key as a **scope array**:

```toml
[dependencies]
para = [
  { version = "^0.1", package = "para/aether" },
  { version = "^0.1", package = "para/db" },
]
```

Every member of the array must share the scope (`para`), and the key is the import root — you
write `use para.aether.…` and `use para.db.…`. A single package needs no array:
`para = [ { version = "^0.1", package = "para/aether" } ]` and
`aether = { version = "^0.1", package = "para/aether" }` both work (the second binds under the
key `aether`). `noeta add --version "^0.1" --package para/aether` makes the edit for you.

The other source forms — a local `{ path = "…" }` tree, a git repo pinned to a
`{ git = "…", tag = "…" }` release or tracking a `branch`/HEAD — are covered in
[the Manifest](Manifest); mix them freely during development, but a *published* package may
depend only via the registry.

## 4 · `[trust]` — when a package is native or adds commands

Packages get no elevated capability by default. A package that runs **native code** (Rust
compiled into your toolchain — `para/db`'s database drivers, `para/p2p`'s networking) must be
authorized explicitly, and so must a package that contributes **`noeta <subcommand>` CLI
commands** (like `para/db`'s `noeta migrate`):

```toml
[trust]
native   = ["para/db"]
commands = ["para/db"]
```

An unauthorized native package or command is refused with an error naming the grant to add —
nothing runs on the strength of appearing in `[dependencies]` alone. `noeta audit` reports the
resulting trust footprint for the whole tree.

## 5 · Run

There is no install step. `noeta run` (and `build`/`check`/`test`) resolves the graph on demand:
fetches each release's source (the registry serves *coordinates*; code comes from the package's
own git repo), verifies its signed [provenance](Package-Provenance), and writes `noeta.lock` —
commit that file. The lock pins every package's version, commit SHA, content hash, and edition,
plus each scope's trust root (trust-on-first-use), so every later build — any machine, any day —
reproduces the same tree, offline once cached.

```noeta ignore
use para.aether.{App, Get}

class HelloController {
    fn new(): HelloController { return HelloController {} }

    #[Get("/hello")]
    fn hello(): string { return "Hello from aether!" }
}

app = App.new()
app.register("HelloController", HelloController.new())
app.discover()
echo app.handle("GET", "/hello", "")
```

```console
$ noeta run src/main.noe
Hello from aether!
```

## 6 · Keeping up

- **`noeta update`** re-resolves everything deliberately: newer versions satisfying your
  requirements, moved branch tips, drifted pins. Nothing else ever moves an existing pin.
- **`noeta upgrade`** updates the *toolchain* itself (never to a prerelease).
- **`noeta audit`** shows what you are actually running: every package's source, the active
  `[trust]` grants, each scope's pinned provenance identity, and any security-advisory hits.

By default everything resolves from `registry.noeta.dev`; private and per-scope registries
(`[registries]`, `NOETA_REGISTRY_URL`, `NOETA_REGISTRY_DIR`) are on
[Package Registries](Package-Registries).
