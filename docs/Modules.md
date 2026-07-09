# Modules & Visibility

Programs span multiple files. Each file declares a `namespace`, imports with `use`, and exposes only what it marks `pub`.

## Namespaces

A file declares its module path with `namespace`:

```noeta
// models.noe
namespace App.Models;

pub class User {
    pub name: string
    pub id: int
    fn new(name: string, id: int): User { return User { name: name, id: id } }
}
```

## Importing with `use`

Import a declaration by its full path. Grouped imports share a prefix:

```noeta ignore
namespace App.Main;

use App.Models.User;                       // single import
use App.Billing.{Invoice, Receipt};        // grouped
use std.{math, json};                       // standard-library modules

customer = User.new("Ada", 7)
echo customer.name                          // Ada
```

## Aliasing an import — `as`

An import can be renamed locally with `as`. This is how a file brings in two types that share a short name from different namespaces — each under its own local name:

```noeta ignore
namespace App.Main;

use App.Money.Amount as Money;      // App.Money.Amount
use App.Geo.Amount as Distance;     // App.Geo.Amount — a wholly distinct type

m = Money { cents: 500 }
d = Distance { meters: 42 }
echo m is Money                     // true
echo m is Distance                  // false — different types despite the shared short name
```

Grouped imports may alias per-name: `use App.Metrics.{Counter as Hits, Gauge}`.

## Type identity across namespaces

Every named type has a **qualified identity** — `App.Models.User`, not just `User` — formed from the namespace it is declared in. That identity is what the language keys on for method dispatch and for `is`/`as` narrowing, so two types with the same short name in different namespaces never conflate. Human-facing output (a value's display, `type_of`, error messages) shows the **short** name, so `App.Models.User` prints as `User`.

Because identity is qualified, a short name only ever clashes *within a single file's local names*: importing two `Amount`s without aliases, or importing a name the file also declares, is the E0020 collision below — resolved by aliasing one of them.

**Native types work identically.** A standard-library type such as `std.id.Uuid` is imported — and aliased — with the same `use`, and carries the same kind of qualified identity (`std.id.Uuid`). A file may declare its own `Counter` while importing a native one under an alias; the two coexist:

```noeta ignore
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
namespace App.Models;

pub class User { pub name: string }    // importable
class Internal { secret: string }      // module-private
```

Field and method visibility inside a type is separate — see [Structs, Classes & Enums](Structs-Classes-and-Enums#fields).

## How it resolves

When you run an entry file, the loader parses it and its sibling `.noe` modules (each declaring its own `namespace`), resolves the entry's `use` declarations to the real `pub` declarations, and merges everything into one program that runs unchanged. Diagnostics from a merged-in module resolve to *that file's* coordinates.

Qualification happens here, at link time, while each module's namespace and its own imports are still known: every type declaration and every reference to one is rewritten to its qualified identity before the modules are merged. A file with no `namespace` keeps bare names, so single-namespace programs are unchanged.

The module graph is incremental: editing one module recomputes only its dependents (see [Architecture & Pipeline](Architecture-and-Pipeline)).

## The standard library

The stdlib is imported the same way, under the `std` namespace:

```noeta
use std.{math, json, fs}

echo math.sqrt(16.0)
echo json.stringify([1, 2, 3])
```

Unused `std` modules are tree-shaken — you only pay for what you import. The full catalog is on [Standard-Library Modules](Standard-Library-Modules).

## See also

- [Standard Library](Standard-Library) — the always-available Ring 1 surface (no import needed).
- [Standard-Library Modules](Standard-Library-Modules) — the `use std.{…}` modules.
