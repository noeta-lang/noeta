# Modules & Visibility

Programs span multiple files. Each file declares a `namespace`, imports with `use`, and exposes only what it marks `pub`. A complete two-file program:

```noeta
// models.noe
namespace App.Models

pub struct User {
    pub name: string
    pub id: int
}

pub fn greet(u: User): string {
    return "hello, ${u.name} (#${u.id})"
}
```

```noeta check
// main.noe
namespace App.Main

use App.Models.{User, greet}

u = User { name: "Ada", id: 7 }
echo greet(u)
```

Running the entry file is the whole build step — `noeta run` links every other `.noe` file in the entry's directory automatically:

```console
$ noeta run main.noe
hello, Ada (#7)
```

The sibling scan is flat: files in subdirectories are **not** picked up (a dependency package, by contrast, is walked recursively — see [Using Packages](Using-Packages)). `noeta check`, `build`, and `test` link the same way.

## Namespaces

A file declares its module path with `namespace` — the first statement in the file. The mapping is by **declaration, not path**: the `namespace` line alone names the module, and the filename and directory layout play no part (`models.noe` above could be renamed freely). Each file is one module, and one namespace belongs to one file — two sibling files must not declare the same namespace, since imports resolve a namespace to a single file and the other file's exports would be unreachable.

## Importing with `use`

Import a declaration by its full path. Grouped imports share a prefix:

```noeta check
namespace App.Main

use App.Models.User;                       // single import
use App.Billing.{Invoice, Receipt};        // grouped
use std.{math, json};                       // standard-library modules

customer = User.new("Ada", 7)
echo customer.name                          // Ada
```

## Qualified references

A declaration can also be referenced by a **module-qualified name** at any use site — a struct literal, an annotation, a call, an enum construction or pattern, even a first-class function value. Importing a whole module binds its last segment as a navigable handle, and once a module is in scope its spelled-out fully-qualified name works too:

```noeta check
namespace App.Main

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

**One name, one meaning.** A value binding may not reuse the local name a `use` binds — `use geometry.vec` followed by `vec = Holder { … }` is a collision (E0020): rename the binding, or alias the import (`use geometry.vec as gv`). So a dotted chain's root is never ambiguous — it is either the module handle or a local, never both. (The same rule governs binders generally — see [no shadowing, E0059](Functions-and-Closures#sealed-functions--the-use--capture-clause).) Type positions are a separate namespace, so a dotted *type* head like `vec.Vec2` never competes with value bindings at all.

## Aliasing an import — `as`

An import can be renamed locally with `as`. This is how a file brings in two types that share a short name from different namespaces — each under its own local name:

```noeta check
namespace App.Main

use App.Money.Amount as Money;      // App.Money.Amount
use App.Geo.Amount as Distance;     // App.Geo.Amount — a wholly distinct type

m = Money { cents: 500 }
d = Distance { meters: 42 }
echo m is Money                     // true
echo m is Distance                  // false — different types despite the shared short name
```

Grouped imports may alias per-name: `use App.Metrics.{Counter as Hits, Gauge}`.

## Type identity across namespaces

Every named type — and every top-level function — has a **qualified identity**, `App.Models.User` or `App.Math.boom`, not just `User` / `boom`, formed from the namespace it is declared in. That identity is what the language keys on for method dispatch, call resolution, and `is`/`as` narrowing, so two types (or two functions) with the same short name in different namespaces never conflate: `use App.Metric.scale as mscale` and `use App.Audio.scale as ascale` bind distinct functions. Human-facing output (a value's display, `type_of`, error messages) shows the **short** name, so `App.Models.User` prints as `User`.

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
namespace App.Models

pub class User { pub name: string }    // importable
class Internal { secret: string }      // module-private
```

Field and method visibility inside a type is separate — see [Structs, Classes & Enums](Structs-Classes-and-Enums#fields).

## How it resolves

The loader parses the entry and its sibling modules, resolves the entry's `use` declarations to the real `pub` declarations (rewriting names to their qualified identities), and merges everything into one program — a diagnostic from a merged-in module still points at that file's own coordinates. The module graph is incremental — editing one module recomputes only its dependents; see [Architecture & Pipeline](Architecture-and-Pipeline) for the pipeline.

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
