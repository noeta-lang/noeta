# Modules & Visibility

Programs span multiple files. **A file is a module, and its path comes from where the file sits**, so there is nothing to declare. You import with `use` and expose only what you mark `pub`. A complete two-file program, in a package named `local/hello`:

```noeta
// src/models.noe  →  the module `hello.models`

pub struct User {
    name: string
    id: int
}

pub fn greet(u: User): string {
    return "hello, ${u.name} (#${u.id})"
}
```

```noeta check
// src/main.noe  →  the module `hello.main`

use hello.models.{User, greet}

u = User { name: "Ada", id: 7 }
echo greet(u)
```

Running the entry file is the whole build step, since `noeta run` links every other `.noe` file in the package automatically:

```console
$ noeta run src/main.noe
hello, Ada (#7)
```

The scan is recursive: your package is walked as a *tree*, so `src/deep/nested.noe` is a module (`hello.deep.nested`) exactly as a dependency package's subdirectories are. `noeta check`, `build`, and `test` link the same way.

The walk stops at three kinds of directory, none of which hold your package's source:

| Directory | Why the walk stops |
|---|---|
| one holding its own `noeta.toml` | a nested package, of which a package's `examples/<app>/` is the standard case |
| a dot-directory | `.git`, and an editor or agent worktree, which is a whole second copy of every module |
| `target/` | the build-output directory |

## Where a module's path comes from

**Module path = the package's import prefix + the file's path inside the package.** The rule in full:

- **The prefix** is how the *importing* program addresses the package:
  - the package you are running, your own, contributes its `[package] root`, or the `package` half of its `[package] name` when it declares none, so `local/hello` gives `hello`;
  - a plain `[dependencies]` entry contributes **the key** you wrote (`codec = { … }` → `codec.…`);
  - a **scope-array** member contributes `{key}.{the package's own root segment}` (`para = [{ package = "para/db" }, …]` → `para.db.…`).
- **The relative path** is the file's path below the package root: every directory, then the file stem, becomes a segment, `/` becomes `.`, and **case is preserved verbatim** (`Helpers/URI.noe` → `Helpers.URI`).
- **A leading `src/` is not a segment.** `src/` is a layout choice, so `src/human.noe` and `human.noe` are both `<prefix>.human`, and moving your sources under `src/` never renames your API.
- **A segment that repeats the one before it collapses.** The package-named root file *is* the module the prefix names: `para-db/db.noe` under the prefix `para.db` is `para.db`, rather than `para.db.db`.
- **Every segment must be a legal identifier**, see [naming](#naming-a-file) below.

| Package | Prefix from | File | Module |
|---|---|---|---|
| your own, `local/hello` | `[package] root` | `src/main.noe` | `hello.main` |
| your own, `local/hello` | `[package] root` | `src/deep/nested.noe` | `hello.deep.nested` |
| your own, `local/hello` | `[package] root` | `src/hello.noe` | `hello` |
| `geometry = { path = "../geometry" }` | the key | `src/vec.noe` | `geometry.vec` |
| `para = { package = "para/api" }` | the key | `middleware.noe` | `para.middleware` |
| `para = [{ package = "para/api" }]` | key + root segment | `middleware.noe` | `para.api.middleware` |
| `para = [{ package = "para/db" }]` | key + root segment | `query.noe` | `para.db.query` |
| `para = [{ package = "para/db" }]` | key + root segment | `db.noe` | `para.db` |

**Renaming the key renames the import path.** A dependency's modules derive under the prefix *you* wrote, so keying `para/cli` as `mycli` gives you `mycli.cli.run`, and nothing inside the package can override that.

**A plain key derives one segment shallower than a scope array**, because the array form adds the package's own root segment. Use the array form when several packages of one scope must sit side by side under one root, which is how the first-party `para/*` packages are published. See [the Manifest](Manifest#dependencies--what-the-package-builds-against).

### Importing your own package's modules

Inside a package, **lead with your `[package] root`**, the segment your own modules derive under:

```noeta ignore
// para-db/query.noe — in a package whose manifest says `root = "db"`
use db.open              // ✅ resolves standalone AND as a dependency
use para.db.open         // ❌ only ever resolved when a consumer keyed it `para`
```

That one spelling works in both builds. It is what your files derive under when the package is built **on its own**, meaning your tests, `noeta check .`, or an example app, and a consumer's build rewrites that leading segment to whatever your modules derive under there. Standalone the prefix *is* that segment, so nothing changes; under `para = [{ package = "para/db" }, …]` your `use db.open` becomes `para.db.open`; under `mydb = { package = "para/db" }` it becomes `mydb.open`. You write neither the consumer's key nor your own scope.

`root` is declared because it is a **naming choice**, and that is the line derivation draws. A module's *path* is a fact about the filesystem, so it is derived and cannot be declared ([E0072](#a-modules-path-cannot-be-declared)); the prefix it derives *under* is your package's name for itself, which nothing on disk knows.

That is also why it is separate from `[package] name`. The name is a **registry coordinate**, indexed, claimed and resolved from a URL, so it may look like one: `noeta-lang/my-toolkit`. The root is a **token spelled in source**, so it must lex as one identifier. Omit `root` and the `package` half is used instead, which then has to lex.

The rewrite touches the **leading segment only**, so everything after it is yours to spell: `use db.query.run` from a file three directories deep still names the module `query.noe` derives.

Two segments are left alone: a `use std.…`, and a native extension's own namespace root. `para/db`'s Rust extension registers the module `para.db`, so its `.noe` modules import `use para.db.Connection` by that literal name and the loader leaves it as written.

### Naming a file

**Convention:** a lowercase, single-word stem such as `models.noe`, `query.noe` or `middleware.noe`, which is what every module in the standard library and the first-party packages uses. Neither the compiler nor a lint enforces it; it is what makes an import path read like the rest of the language. It sits alongside the rest of the ecosystem's naming in [Conventions](Conventions).

**Rule:** every segment, each directory name and the file stem, has to lex as exactly one identifier, because every one of them is spelled out in somebody's `use`. A stem that cannot is **E0074**, reported against the file with the rename to make:

```text
[E0074] `my-utils` cannot be part of a module path — a module's path is derived from where its
        file sits, and every segment of it has to be spellable in a `use`
   help: rename it to `my_utils`
```

The hint is advice, and nothing is applied silently: mapping `my-utils` to `my_utils` behind your back would give one module two spellings. A keyword is refused for the same reason (`class.noe`, which no `use` could name), as is a stem starting with a digit (`2fa.noe` → `_2fa`).

**One name is reserved.** **`self`** is the method receiver, and it is refused with the same code for a different reason: it spells perfectly well, and the language has already taken it. A module of that name would collide with the receiver, because importing `pkg.self` binds the handle `self`, after which `self.n` inside a method reads the receiver's field while `self.hi()` reads the module's function. The [one name, one meaning](#qualified-references) rule cannot catch that, since a receiver is bound by the method rather than by a `use`.

### One path, one module

Two files that derive the same path are **E0073**, naming both:

```text
[E0073] two files derive the module path `hello.api`: `src/api.noe` and `src/api/api.noe`
   help: one module path is one module — rename or move one of the files so their paths differ
```

That is the collapse rule biting: `src/api/api.noe`'s stem repeats the directory it sits in, so it lands on `hello.api` alongside `src/api.noe`. Pick one of the two spellings for the module, either a file or a directory with a root file. Reporting the collision here keeps one file's exports out of the other's, where the failure would surface against whoever imported them.

### Case is preserved

`Helpers/URI.noe` is `Helpers.URI`, and `use pkg.helpers.uri` does not find it.

The loader **scans the directory and matches derived strings**, so the filesystem's own case rules never enter the comparison and a mis-cased `use` fails identically on every platform, at check time. It also keeps one rule end to end: `Uuid` is not `uuid` anywhere else in the language, and lowercasing module segments alone would make the path fuzzy while the imported item stayed exact.

### A module's path cannot be declared

Inside a package, a module's path is derived and **only** derived. A file under a `noeta.toml` that declares a `namespace` is **E0072**, whatever it says, since its path already comes from where it sits:

```text
[E0072] a module's path is derived from where its file sits, so it cannot be declared
   help: delete this line: this file's path derives as `hello.models`, and moving the file is how
         you rename the module
```

Deleting the line is the whole fix, as long as the file sits where the declaration says it lives. Where the two disagree, the declaration is the wrong half, so move the file.

**One exception.** A loose script with *no* manifest has no package, hence no prefix, hence nothing to derive a path from. There a `namespace` declaration is how a sibling module gets a name, and it is accepted. The rule is scoped to files whose path *can* be derived. For derived paths, `noeta init` gives you a manifest.

### Derivation needs a package

A prefix comes from a manifest, so a file with **no `noeta.toml` above it** has nothing to derive from. A lone script run straight out of a directory keeps whatever it declares, and nothing derives. That is what confines `noeta run scratch.noe` to the one file, and it is why the single-file samples on these pages need no package to be valid programs.

### The three diagnostics

| Code | When | Fix |
|---|---|---|
| **E0072** | a file **in a package** declares a `namespace` — a derived path cannot be declared | delete the line; move the file if you meant to rename the module |
| **E0073** | two files derive the same module path | rename or move one — one path is one module |
| **E0074** | a directory name or file stem is not a legal identifier segment | rename it to the spelling the help offers |

All three come from the loader, so `check`, `run`, `build`, and `test` report them identically. A program that fails to name its modules never gets as far as type-checking.

## Importing with `use`

Import a declaration by its full path. Grouped imports share a prefix:

```noeta check
use hello.models.User;                      // single import
use hello.billing.{Invoice, Receipt};       // grouped
use std.{math, json};                       // standard-library modules

customer = User.new("Ada", 7)
echo customer.name                          // Ada
```

## Qualified references

A declaration can also be referenced by a **module-qualified name** at any use site. Importing a whole module binds its last segment as a navigable handle, and a module in scope also answers to its spelled-out fully-qualified name:

```noeta check
use geometry.vec;                              // the whole module: binds `vec`

a = vec.Vec2 { x: 1, y: 2 }                    // qualified struct literal
b: vec.Vec2 = vec.Vec2 { x: 3, y: 4 }          // qualified annotation
c = vec.add(a, b)                              // qualified call
g = geometry.vec.Vec2 { x: 5, y: 6 }           // the spelled-out FQN, same identity
f = geometry.vec.add                           // a first-class fn value, FQN too
s = vec.Shape.Circle(7)                        // qualified enum construction
r = match s {
    vec.Shape.Circle(radius) => radius,        // qualified pattern
    _ => 0,
}
```

A whole-module import aliases as usual (`use geometry.vec as gv` → `gv.Vec2`) and merges only `pub` declarations. In `use a.b`, an item `b` exported by module `a` wins over a module named `a.b`.

**Qualified references require an import.** A file's dependency set stays readable from its `use` block, so a spelled-out FQN with no import is an error naming the line to add:

```text
[E0019] qualified reference `geometry.vec.Vec2` requires an import — add `use geometry.vec`
```

A non-`pub` declaration referenced by qualified name reports `` `secret` is private to module `geometry.vec` ``.

**A handle means what its own file imported.** A `use` binds *in one file*, and that holds across packages: a library that writes `use std.http.url` calls `std.http.url` even when the application also depends on a package whose native extension registers a module `url` of its own.

An alias (`use std.http.url as codec`) is honored at run time as the checker reads it.

That holds for an imported **type** too: `use std.http.Framing as F` makes `F` the name your file writes, while a value of it still carries `Framing`, the name a native-returned value stamps and a pattern compares against. `F.Sse` therefore equals the very same variant a file that imported it unaliased builds.

**One name, one meaning.** A value binding may not reuse the local name a `use` binds, so `use geometry.vec` followed by `vec = Holder { … }` is a collision (E0020): rename the binding, or alias the import. (The same rule governs binders generally: [no shadowing, E0059](Functions-and-Closures#sealed-functions--the-use--capture-clause).) Type positions are a separate namespace, so a dotted *type* head like `vec.Vec2` never competes with a value binding.

## Aliasing an import — `as`

An import can be renamed locally with `as`. This is how a file brings in two types that share a short name from different namespaces, each under its own local name:

```noeta check
use shop.money.Amount as Money;     // shop.money.Amount
use shop.geo.Amount as Distance;    // shop.geo.Amount — a wholly distinct type

m = Money { cents: 500 }
d = Distance { meters: 42 }
echo m is Money                     // true
echo m is Distance                  // false — different types despite the shared short name
```

Grouped imports may alias per-name: `use shop.metrics.{Counter as Hits, Gauge}`.

## Type identity across modules

Every named type, and every top-level function, has a **qualified identity**: `shop.models.User` or `shop.math.boom`, formed from the path of the module it is declared in, which is to say from where its file sits.

That identity is what the language keys on for method dispatch, call resolution, and `is`/`as` narrowing, so two types or two functions sharing a short name in different modules stay distinct: `use shop.metric.scale as mscale` and `use shop.audio.scale as ascale` bind different functions. Human-facing output, meaning a value's display and diagnostics, shows the **short** name, so a `shop.models.User` value prints as `User {…}` and a type error names it `User`.

**Reflection is the exception, deliberately.** A `Type` value carries the *qualified* identity, because that identity is the key the name-keyed queries are stored under: `type_of(u)` inside `shop.models` is `Type.Struct(shop.models.User, [])`, and `type_name::<User>()` is `"shop.models.User"`. Comparing a reflected name against a hand-written `"User"` therefore fails for every type declared in a module, so write [`type_name::<User>()`](Attributes-and-Reflection#type_namet-string) and let the compiler produce the string.

Because identity is qualified, a short name clashes only *within a single file's local names*: importing two `Amount`s without aliases, or importing a name the file also declares, is the E0020 collision below, resolved by aliasing one of them.

**Native types work identically.** A standard-library type such as `std.id.Uuid` is imported, and aliased, with the same `use`, and carries the same kind of qualified identity (`std.id.Uuid`). A file may declare its own `Counter` while importing a native one under an alias; the two coexist:

```noeta check
use std.id                           // the module handle, for `id.uuid()`
use std.id.Uuid as NativeId          // the native type, renamed locally

struct Uuid { tag: int }             // your own, unrelated type

n: NativeId = id.uuid()
mine = Uuid { tag: 5 }
echo n is NativeId                   // true
echo mine is Uuid                    // true, and `mine is NativeId` is false
```

## Visibility — `pub`

A declaration is **module-private by default**. Only `pub` items can be imported from another module.

- Importing a private (or non-existent) export is E0019.
- A genuinely unknown type name is E0013.
- Importing a name the file *also* declares locally (or imports twice) is a collision, E0020.

```noeta
pub class User { pub name: string }    // importable
class Internal { secret: string }      // module-private
```

Field and method visibility inside a type is separate, see [Structs, Classes & Enums](Structs-Classes-and-Enums#fields).

## How it resolves

The loader walks the package, derives each file's module path from its location, resolves every `use` to the real `pub` declarations (rewriting names to their qualified identities), and merges everything into one program. A diagnostic from a merged-in module still points at that file's own coordinates. Deriving happens before checking, so E0072/E0073/E0074 are reported by `check` exactly as by `run`. The module graph is incremental, and editing one module recomputes only its dependents; see [Architecture & Pipeline](Architecture-and-Pipeline) for the pipeline.

## The standard library

The stdlib is imported the same way, under the `std` namespace:

```noeta
use std.{math, json, fs}

echo math.sqrt(16.0)
echo json.stringify([1, 2, 3])
```

Unused `std` modules are tree-shaken, so you pay only for what you import. The full catalog is the [standard library reference](Std).

## Namespace groups

A namespace that holds several submodules can be imported as a single **group**, one handle you dot into, instead of importing each leaf. `std.http` is the canonical example: it splits into `http.client` (the request client) and `http.server`, and you can bring in the whole group and reach either through it:

```noeta check
use std.http

r = http.client.get("https://example.com")?  // the `client` submodule
echo r.status()
echo r is http.Response                        // a type reached through the group too
```

`http.client.get(...)` resolves the `client` submodule at each call site and dispatches exactly as the leaf form `use std.http.client; client.get(...)`. The group handle is pure compile-time resolution, so it costs nothing at runtime and tree-shaking still sheds `http.client`'s dependencies from a server-only build. Types work the same way: `http.Response` resolves to the same `std.http.Response` identity as the leaf import `use std.http.Response`. Any extension root whose modules share a prefix can be grouped this way, and the leaf forms keep working unchanged.

Reaching for a member the group does not have is a compile error, with a suggestion:

```noeta error
use std.http
r = http.get("...")     // E0005: namespace `http` has no member `get`
echo "unreachable"
```

## Unresolved imports are errors

A `use` that resolves to nothing is a compile-time error on both backends, so `check` and `run` agree and a build never ships a binary that fails at startup. Each carries a "did you mean `X`?" hint when a valid target is a near miss:

| What is misspelled | Example | Report |
|---|---|---|
| a std module or member | `use std.htpt` | **E0019**, did you mean `http`? |
| a module in your own project | `use App.Modles.User` | **E0019**, did you mean `App.Models`? |
| a dependency package | `use imgtx.fx` | **E0019**, did you mean `imgfx`? (when `imgfx` is a declared dependency) |

A single file checked in isolation stays lenient about names its siblings or dependencies would supply. The strict check applies once the whole project, with its resolved dependency graph, is linked.

## See also

- [Built-ins (Ring 1)](Standard-Library) — the always-available surface (no import needed).
- [Standard library reference](Std) — the generated per-module API pages for `use std.{…}`.
