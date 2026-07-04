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
