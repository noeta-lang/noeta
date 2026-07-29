# Modules & Visibility

Programs span multiple files. **A file is a module, and its path comes from where the file sits** — there is nothing to declare. You import with `use` and expose only what you mark `pub`. A complete two-file program, in a package named `local/hello`:

```noeta
// src/models.noe  →  the module `hello.models`

pub struct User {
    pub name: string
    pub id: int
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

Running the entry file is the whole build step — `noeta run` links every other `.noe` file in the package automatically:

```console
$ noeta run src/main.noe
hello, Ada (#7)
```

The scan is recursive: your package is walked as a *tree*, so `src/deep/nested.noe` is a module (`hello.deep.nested`) exactly as a dependency package's subdirectories are. `noeta check`, `build`, and `test` link the same way.

The walk stops at three kinds of directory, none of which are your package's source: a directory holding **its own `noeta.toml`** (a nested package — a package's `examples/<app>/` is the standard case), a **dot-directory** (`.git`, and an editor or agent worktree, which is a whole second copy of every module), and the build-output directory `target/`.

## Where a module's path comes from

**Module path = the package's import prefix + the file's path inside the package.** The rule in full:

- **The prefix** is how the *importing* program addresses the package:
  - the package you are running — your own — contributes the `package` half of its `[package] name`, so `local/hello` gives `hello`;
  - a plain `[dependencies]` entry contributes **the key** you wrote (`codec = { … }` → `codec.…`);
  - a **scope-array** member contributes `{key}.{the package's own root segment}` (`para = [{ package = "para/db" }, …]` → `para.db.…`).
- **The relative path** is the file's path below the package root: every directory, then the file stem, becomes a segment, `/` becomes `.`, and **case is preserved verbatim** (`Helpers/URI.noe` → `Helpers.URI`).
- **A leading `src/` is not a segment.** `src/` is a layout choice, so `src/human.noe` and `human.noe` are both `<prefix>.human` — moving your sources under `src/` never renames your API.
- **A segment that repeats the one before it collapses.** The package-named root file *is* the module the prefix names, not a child of it: `para-db/db.noe` under the prefix `para.db` is `para.db`, not `para.db.db`.
- **Every segment must be a legal identifier** — see [naming](#naming-a-file) below.

| Package | Prefix from | File | Module |
|---|---|---|---|
| your own, `local/hello` | `[package] name` | `src/main.noe` | `hello.main` |
| your own, `local/hello` | `[package] name` | `src/deep/nested.noe` | `hello.deep.nested` |
| your own, `local/hello` | `[package] name` | `src/hello.noe` | `hello` |
| `geometry = { path = "../geometry" }` | the key | `src/vec.noe` | `geometry.vec` |
| `para = [{ package = "para/db" }]` | key + root segment | `query.noe` | `para.db.query` |
| `para = [{ package = "para/db" }]` | key + root segment | `db.noe` | `para.db` |

**The key is real.** Because a dependency's modules derive under the prefix *you* wrote, renaming the key renames the import path: keying `para/cli` as `mycli` gives you `mycli.cli.run`, and nothing inside the package can override that. A package's internal file names are still its public API surface — that is the point — but the *root* they hang under is the consumer's to choose.

**A plain key derives one segment shallower than a scope array.** `para = { package = "para/api" }` makes `middleware.noe` into `para.middleware`; the array form `para = [{ package = "para/api" }]` makes it `para.api.middleware`, because the array form adds the package's own root segment. When several packages of one scope must sit side by side under one root — which is exactly how the first-party `para/*` packages are published — use the array form. See [the Manifest](Manifest#dependencies--what-the-package-builds-against).

### Naming a file

**Convention:** a lowercase, single-word stem — `models.noe`, `query.noe`, `middleware.noe` — which is what every module in the standard library and the first-party packages uses. The compiler does not enforce it, and neither does a lint; it is what makes an import path read like the rest of the language.

**Rule:** every segment — each directory name and the file stem — has to lex as exactly one identifier, because every one of them is spelled out in somebody's `use`. A stem that cannot be is **E0074**, reported against the file with the rename to make:

```text
[E0074] `my-utils` cannot be part of a module path — a module's path is derived from where its
        file sits, and every segment of it has to be spellable in a `use`
   help: rename it to `my_utils`
```

The hint is advice; nothing is applied silently. Mapping `my-utils` → `my_utils` behind your back would give one module two spellings, which is precisely what deriving the path exists to remove. A keyword is refused for the same reason (`class.noe` — no `use` could name it), as is a stem starting with a digit (`2fa.noe` → `_2fa`).

### One path, one module

Two files that derive the same path are **E0073**, naming both:

```text
[E0073] two files derive the module path `hello.api`: `src/api.noe` and `src/api/api.noe`
   help: one module path is one module — rename or move one of the files so their paths differ
```

That is the collapse rule biting: `src/api/api.noe`'s stem repeats the directory it sits in, so it lands on `hello.api` alongside `src/api.noe`. Pick one of the two spellings for the module — either a file or a directory with a root file, not both. The collision used to be silent: the second file's exports simply vanished, and the failure surfaced against whoever imported them.

### Case is preserved

`Helpers/URI.noe` is `Helpers.URI`, and `use pkg.helpers.uri` does not find it.

The usual objection is PSR-4's cross-platform wound, and it does not apply here. PHP's autoloader *builds a path out of the name and asks the filesystem to open it*, which is where a case-insensitive filesystem bites — the same code resolves on macOS and 404s on Linux. Noeta's loader does the opposite: it **scans the directory and matches derived strings**, so the filesystem's case rules never enter the comparison and a mis-cased `use` fails identically on every platform, at check time. It also keeps one rule end to end — `Uuid` is not `uuid` anywhere else in the language either, and lowercasing module segments alone would make the path fuzzy while the imported item stayed exact.

### `namespace` is redundant

A file may still open with a `namespace` declaration, but it is a **restatement of the derived path, not a definition of it**. A declaration that disagrees is **E0072**:

```text
[E0072] this module declares `namespace App.Models`, but its path derives as `hello.models`
   help: a module's path is the package's import prefix plus the file's path inside the package —
         delete the declaration, or move the file to where it says it lives
```

New code should not write one; it is being removed from the language. `namespace` is not *gone* — a declaration that agrees with the derivation is still accepted, so an existing package keeps compiling while its declarations are deleted file by file. It just no longer decides anything.

### Derivation needs a package

A prefix comes from a manifest, so a file with **no `noeta.toml` above it** has nothing to derive from. A lone script run straight out of a directory is not silently made into a module of whatever tree it happens to stand in — nothing derives, and whatever the file declares stands. That is what keeps `noeta run scratch.noe` from swallowing the tree it happens to be sitting in, and it is why the single-file samples on these pages need no package to be valid programs.

### The three diagnostics

| Code | When | Fix |
|---|---|---|
| **E0072** | a `namespace` declaration disagrees with the derived path | delete the declaration, or move the file to where it claims to live |
| **E0073** | two files derive the same module path | rename or move one — one path is one module |
| **E0074** | a directory name or file stem is not a legal identifier segment | rename it to the spelling the help offers |

All three come from the loader, so `check`, `run`, `build`, and `test` report them identically — a program that fails to name its modules never gets as far as type-checking.

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

A declaration can also be referenced by a **module-qualified name** at any use site — a struct literal, an annotation, a call, an enum construction or pattern, even a first-class function value. Importing a whole module binds its last segment as a navigable handle, and once a module is in scope its spelled-out fully-qualified name works too:

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

A whole-module import follows the usual aliasing (`use geometry.vec as gv` → `gv.Vec2`) and merges only `pub` declarations. In `use a.b`, an item `b` exported by module `a` wins over a module named `a.b`.

**Qualified references require an import.** A file's dependency set stays fully readable from its `use` block, so a spelled-out FQN with no import is never a silent implicit import — it is a targeted error carrying the exact line to add:

```text
[E0019] qualified reference `geometry.vec.Vec2` requires an import — add `use geometry.vec`
```

(and referencing a non-`pub` declaration by qualified name reports `` `secret` is private to module `geometry.vec` ``).

**A handle means what its own file imported.** A `use` binds *in one file*, and that holds across packages: a library that writes `use std.http.url` calls `std.http.url` even when the application also depends on a package whose native extension registers a module `url` of its own. The two never see each other's handles — the linker binds every imported unit's handle under its module's qualified identity, so a leaf name is never the thing two files compete for. The same is true of an alias (`use std.http.url as codec`), which is honored at run time exactly as the checker reads it.

That holds for an imported **type** too, and there the binding and the identity are deliberately two different things: `use std.http.Framing as F` makes `F` the name your file writes, while a value of it still carries `Framing` — the name a native-returned value stamps and the one a pattern compares against. So `F.Sse` builds, matches, and equals the very same variant a file that imported it unaliased builds, and neither file has to know how the other spelled it.

**One name, one meaning.** A value binding may not reuse the local name a `use` binds — `use geometry.vec` followed by `vec = Holder { … }` is a collision (E0020): rename the binding, or alias the import (`use geometry.vec as gv`). So a dotted chain's root is never ambiguous — it is either the module handle or a local, never both. (The same rule governs binders generally — see [no shadowing, E0059](Functions-and-Closures#sealed-functions--the-use--capture-clause).) Type positions are a separate namespace, so a dotted *type* head like `vec.Vec2` never competes with value bindings at all.

## Aliasing an import — `as`

An import can be renamed locally with `as`. This is how a file brings in two types that share a short name from different namespaces — each under its own local name:

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

Every named type — and every top-level function — has a **qualified identity**, `shop.models.User` or `shop.math.boom`, not just `User` / `boom`, formed from the path of the module it is declared in — which is to say, from where its file sits. That identity is what the language keys on for method dispatch, call resolution, and `is`/`as` narrowing, so two types (or two functions) with the same short name in different modules never conflate: `use shop.metric.scale as mscale` and `use shop.audio.scale as ascale` bind distinct functions. Human-facing output — a value's display and diagnostics — shows the **short** name, so a `shop.models.User` value prints as `User {…}` and a type error names it `User`.

**Reflection is the exception, deliberately.** A `Type` value carries the *qualified* identity, because that identity is the key the name-keyed queries are stored under: `type_of(u)` inside `shop.models` is `Type.Struct(shop.models.User, [])`, and `type_name::<User>()` is `"shop.models.User"`. Comparing a reflected name against a hand-written `"User"` therefore fails for every type declared in a module — write [`type_name::<User>()`](Attributes-and-Reflection#type_namet-string) and let the compiler produce the string.

Because identity is qualified, a short name only ever clashes *within a single file's local names*: importing two `Amount`s without aliases, or importing a name the file also declares, is the E0020 collision below — resolved by aliasing one of them.

**Native types work identically.** A standard-library type such as `std.id.Uuid` is imported — and aliased — with the same `use`, and carries the same kind of qualified identity (`std.id.Uuid`). A file may declare its own `Counter` while importing a native one under an alias; the two coexist:

```noeta check
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

Field and method visibility inside a type is separate — see [Structs, Classes & Enums](Structs-Classes-and-Enums#fields).

## How it resolves

The loader walks the package, derives each file's module path from its location, resolves every `use` to the real `pub` declarations (rewriting names to their qualified identities), and merges everything into one program — a diagnostic from a merged-in module still points at that file's own coordinates. Deriving happens before checking, so E0072/E0073/E0074 are reported by `check` exactly as by `run`. The module graph is incremental — editing one module recomputes only its dependents; see [Architecture & Pipeline](Architecture-and-Pipeline) for the pipeline.

## The standard library

The stdlib is imported the same way, under the `std` namespace:

```noeta
use std.{math, json, fs}

echo math.sqrt(16.0)
echo json.stringify([1, 2, 3])
```

Unused `std` modules are tree-shaken — you only pay for what you import. The full catalog is the [standard library reference](Std).

## Namespace groups

A namespace that holds several submodules can be imported as a single **group** — one handle you dot into — instead of importing each leaf. `std.http` is the canonical example: it splits into `http.client` (the request client) and `http.server`, but you can bring in the whole group and reach either through it:

```noeta check
use std.http

r = http.client.get("https://example.com")?  // the `client` submodule
echo r.status()
echo r is http.Response                        // a type reached through the group too
```

`http.client.get(...)` resolves the `client` submodule at each call site and dispatches exactly as the leaf form `use std.http.client; client.get(...)` — the group handle is pure compile-time resolution, so it costs nothing at runtime and tree-shaking still sheds `http.client`'s dependencies from a server-only build. Types work the same way: `http.Response` resolves to the same `std.http.Response` identity as the leaf import `use std.http.Response`. Any extension root whose modules share a prefix can be grouped this way; the leaf forms keep working unchanged.

Reaching for a member the group does not have is a compile error, with a suggestion:

```noeta error
use std.http
r = http.get("...")     // E0005: namespace `http` has no member `get`
echo "unreachable"
```

## Unresolved imports are errors

A `use` that resolves to nothing is a compile-time error on both backends — `check` and `run` agree, and a build never ships a binary that fails at startup. Each carries a "did you mean `X`?" hint when a valid target is a near miss:

- a mistyped std module or member — `use std.htpt` → **E0019** (did you mean `http`?);
- a missing module in your own project — `use App.Modles.User` → **E0019** (did you mean `App.Models`?);
- a mistyped or undeclared dependency package — `use imgtx.fx` → **E0019** (did you mean `imgfx`?), when `imgfx` is a declared dependency.

A single file checked in isolation stays lenient about names its siblings or dependencies would supply; the strict check applies once the whole project (with its resolved dependency graph) is linked.

## See also

- [Standard Library](Standard-Library) — the always-available Ring 1 surface (no import needed).
- [Standard library reference](Std) — the generated per-module API pages for `use std.{…}`.
