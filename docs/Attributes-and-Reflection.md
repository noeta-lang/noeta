# Attributes & Reflection

The language distinguishes **codegen directives** (`@…`) from **data attributes** (`#[…]`), and exposes a small runtime reflection surface. The one-line rule:

> `@` = the compiler generates or registers something; `#[…]` = inert data metadata is attached.

## The four decorator directives

There is a fixed set of four `@` decorators that annotate *declarations*. (These are distinct from the `@test`/`@bench`/`@doc`/`@debug` [dev-tier blocks](Documentation-and-Tiers), which gate content rather than decorate declarations.)

### `@derive(...)` — synthesize trait impls

Generates trait implementations from a type's shape. Covered in [Generics & Traits](Generics-and-Traits#derive--synthesized-implementations). The derivable set is closed: `Equatable`, `Comparable`, `Display`, `Clone`, `Serialize<Format>`.

### `@attribute` — mark a struct usable as `#[...]`

A `#[Foo(...)]` attribute *is* a struct constructed in annotation position. The struct opts in by being marked `@attribute`:

```lang
@attribute
struct Route { path: string  method: string = "GET" }

#[Route("/users")]                 // path: "/users", method defaults to "GET"
struct Users { id: int }

#[Route("/admin", method: "POST")]
fn admin_handler(): void { /* ... */ }
```

- Attributes are **structs, not classes** (a struct has one canonical all-fields construction).
- Arguments map to fields — positional in declaration order, or named. A field with a default is optional.
- Using an unmarked struct (or a class/enum) as an attribute is E0029.
- Placement can be constrained by listing target kinds — `@attribute(Method, Function)` — and a misplaced attribute is E0030. The kinds are `Struct`, `Class`, `Enum`, `Function`, `Method`, `Field`, `Variant`.
- Arguments are a **constant literal tree** — scalars, lists, maps, sets, enum values, nested struct literals, and a bare type name (which becomes a reflection `Type` value). A non-literal argument (e.g. `1 + 2`) is E0003.

The `#[Skip]` / `#[Name]` / `#[Group]` / `#[Data]` attributes used by the [test runner](Testing) are exactly such prelude `@attribute` structs.

### `@role(Enum.Variant)` — a semantic role tag

Rides on an `@attribute` struct and confers a typed architectural role on every declaration the attribute annotates, indexed at build time (zero runtime cost). Only a struct marked `@attribute` may carry `@role`, and the variant must be fieldless.

```lang
@attribute(Function, Method)
@role(Semantic.EntryPoint)
struct Route { path: string }
```

### `@semantic` — promote an enum to a role vocabulary

Marks an **enum** (only) as a source of role variants. The language ships a built-in `Semantic` enum (`EntryPoint`, `PersistenceBoundary`, `TrustBoundary`, `Sink`, `Layer`); any project enum marked `@semantic` becomes role-eligible:

```lang
@semantic enum WebRole { Controller; Middleware; ErrorHandler }
```

Applying `@role`/`@semantic` to the wrong declaration kind is E0031.

## The reflection surface

A handful of prelude functions expose type and metadata at runtime.

### `type_of(value): Type`

Returns the value's runtime head-constructor as the prelude `Type` ADT, which you can `match`:

```lang
echo match type_of(5) {
    Type.Int    => "int",
    Type.String => "string",
    _           => "other",
}
```

`Type` variants include `Type.Int`, `Type.Float`, `Type.Bool`, `Type.String`, `Type.Bytes`, `Type.Dyn`, `Type.List(inner)`, `Type.Map(k, v)`, `Type.Option(inner)`, `Type.Struct(name, _)`, `Type.Enum(name, _)`, `Type.Class(name, _)`, and `Type.Named(name, _)`. Collection literals carry their resolved element type as a runtime tag that survives a `dyn` launder (a content-changing op like `.set` drops the tag to head-only).

### `attributes_of::<T>(): List<Attributed<T>>`

Materializes every `#[T(...)]` attribute in the program — each entry's `.value` is a real `T`, and `.target` is the annotated declaration's name:

```lang
routes = attributes_of::<Route>()
for r in routes {
    echo "${r.target} -> ${r.value.path}"
}
```

### `roles_of(): List<RoleBinding>`

The compile-time `(declaration, role)` index built from `@role(...)` tags — each binding has a `.target` and a `.role`.

### `invoke(recv, name, args): Result<dyn, dyn>`

Fallible dispatch by name — `recv` is a value (→ an instance method) or a bare type name (→ an associated function). Returns `Err` on an unknown name, a non-string name, or an arity mismatch:

```lang
echo match invoke(Shape.new(2, 3), "area", []) {
    Ok(v)  => "area = ${v}",
    Err(e) => "no such method",
}
```

## Where this is headed

The reflection manifest — declarations, their `#[…]` attributes, and their `@role`/`@semantic` tags — is designed to back an agentic tooling surface (querying a program's architectural graph over MCP). That server does not ship yet; see [Editor & AI Tooling](Editor-and-AI-Tooling) for the honest status.
