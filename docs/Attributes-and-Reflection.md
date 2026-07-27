# Attributes & Reflection

The language distinguishes **codegen directives** (`@…`) from **data attributes** (`#[…]`), and exposes a small runtime reflection surface. The one-line rule:

> `@` = the compiler generates or registers something; `#[…]` = inert data metadata is attached.

## The decorator directives

Four `@` decorators attach metadata to or drive codegen on a *declaration* — `@derive`, `@attribute`, `@role`, and `@semantic`. (The layout directive `@packed` and the `@test`/`@bench`/`@doc`/`@debug` [dev-tier blocks](Documentation-and-Tiers) also use `@` but do different jobs — see [Other `@` directives](#other--directives) below.)

### `@derive(...)` — synthesize trait impls

Generates trait implementations from a type's shape. Covered in [Generics & Traits](Generics-and-Traits#derive--synthesized-implementations). The **built-in recipe set** is closed — `Equatable`, `Comparable`, `Display`, `Error`, `Clone`, `Serialize<Format>` — but **user traits are derivable too**: a fully-defaulted trait's defaults are adopted wholesale (the `Inspectable` example below), and a required method can be bridged onto a field/method or delegated `via:` a field. What `@derive` never does is run arbitrary code — every synthesized method is a mechanical bridge, forward, or default.

### `@attribute` — mark a struct usable as `#[...]`

A `#[Foo(...)]` attribute *is* a struct constructed in annotation position. The struct opts in by being marked `@attribute`:

```noeta
@attribute
struct Route { path: string  method: string = "GET" }

#[Route("/users")]                 // path: "/users", method defaults to "GET"
struct Users { id: int }

#[Route("/admin", method: "POST")]
fn admin_handler(): void { /* handle the request */ }
```

- Attributes are **structs, not classes** (a struct has one canonical all-fields construction).
- Arguments map to fields — positional in declaration order, or named. A field with a default is optional.
- Using an unmarked struct as an attribute is E0029. Writing `@attribute` itself on a class or
  enum is a misplaced directive, E0054.
- Placement can be constrained by listing target kinds — `@attribute(Method, Function)` — and a misplaced attribute is E0030. The kinds are `Struct`, `Class`, `Enum`, `Function`, `Method`, `Field`, `Variant`.
- Arguments are a **constant literal tree** — scalars, lists, maps, sets, enum values, nested struct literals, and a type reference (which becomes a reflection `Type` value). A non-literal argument (e.g. `1 + 2`) is E0003.
- A type-reference argument keeps its **generic arguments** at full fidelity — `#[Builds(target: List<int>)]` reflects as `Type.List(Type.Int)`, not an erased `Type.List(Type.Dyn)` — and is validated exactly like a type annotation anywhere else: an unknown name inside it is E0013, and a built-in constructor applied at the wrong arity (`List<int, string>`) is E0058.

The `#[Skip]` / `#[Name]` / `#[Group]` / `#[Data]` attributes used by the [test runner](Testing) are exactly such prelude `@attribute` structs.

### `@role(Enum.Variant)` — a semantic role tag

Roles exist to make **architecture queryable**: they tag declarations with typed architectural meaning — this is an entry point, that crosses a trust boundary — that tooling can index and answer questions about, from `roles_of()` in-language to the manifest `noeta mcp` serves to AI agents (see [Where this is headed](#where-this-is-headed)).

`@role` rides on an `@attribute` struct and confers a typed architectural role on every declaration the attribute annotates, indexed at build time (zero runtime cost). Only a struct marked `@attribute` may carry `@role`, and the variant must be fieldless.

```noeta
@attribute(Function, Method)
@role(Semantic.EntryPoint)
struct Route { path: string }
```

### `@semantic` — promote an enum to a role vocabulary

Marks an **enum** (only) as a source of role variants. The language ships a built-in `Semantic` enum (`EntryPoint`, `PersistenceBoundary`, `TrustBoundary`, `Sink`, `Layer`); any project enum marked `@semantic` becomes role-eligible:

```noeta
@semantic enum WebRole { Controller; Middleware; ErrorHandler }
```

Applying `@role`/`@semantic` to the wrong declaration kind is E0054 — the one code every
misplaced directive reports, whichever directive it is.

### Other `@` directives

Two more directive families use the `@` sigil but are not decorators in this four-set:

- **`@packed` / `@packed(Layout.Column)`** — a *layout* directive marking a struct as a packed value type (flat or column-major storage). See [Fixed-Width Integers & Packed Types](Fixed-Width-Integers#packed-value-types--packed).
- **`@test` / `@bench` / `@doc` / `@debug`** — *dev-tier* blocks that gate co-located content. See [Documentation & Dev Tiers](Documentation-and-Tiers).

## The reflection surface

A handful of prelude functions expose type and metadata at runtime — no import needed.

### `fields_of(value)` — value-level field reflection

`fields_of(value)` returns a struct/class instance's fields as `List<FieldEntry>` — each `{ name: string, value: dyn }`, in declaration order (any other value yields the empty list). It is the value-level counterpart of `type_of`, and what lets a fully-defaulted trait implement *structural* behavior over `self` in pure Noeta — no macro system:

```noeta check
trait Inspectable {
    fn inspect(): string {
        mut out = "{"
        for f in fields_of(self) { out = out ~ " " ~ f.name ~ ": " ~ f.value }
        return out ~ " }"
    }
}
@derive(Inspectable)
struct User { name: string; id: int }
```

### `type_of(value): Type`

Returns the value's runtime head-constructor as the prelude `Type` ADT, which you can `match`:

```noeta
echo match type_of(5) {
    Type.Int    => "int",
    Type.String => "string",
    _           => "other",
}
```

`Type` variants include the scalars `Type.Int`, `Type.Float`, `Type.F32`, `Type.F64`, `Type.IntN(bits, signed)`, `Type.Bool`, `Type.String`, `Type.Bytes`, `Type.Unit`, `Type.Dyn`; the containers `Type.List(inner)`, `Type.Set(inner)`, `Type.Map(k, v)`, `Type.Option(inner)`, `Type.Result(ok, err)`; `Type.Fn(params, ret)` and `Type.Union(members)`; the trait object `Type.DynTrait(name)`; and the nominals `Type.Struct(name, args)`, `Type.Enum(name, args)`, `Type.Class(name, args)`, `Type.Named(name, args)`. Collection literals carry their resolved element type as a runtime tag that survives a `dyn` launder (a content-changing op like `.set` drops the tag to head-only).

### `params_of(name): List<ParamInfo>`

Reflects a **top-level function's signature** by name — one `ParamInfo` per parameter, in declaration order: `{ name: string, type: Type, optional: bool, attrs: List<dyn> }`. `type` is the parameter's *declared* type as the same `Type` ADT `type_of` returns, `optional` reports whether a call may omit the parameter (it declared a default), and `attrs` holds the parameter's own `#[...]` attribute instances:

```noeta
fn scale(factor: f64, xs: List<f32>, ns: List<i32>, label: string = "x"): void { return }

for p in params_of("scale") {
    echo "${p.name}: ${p.type} optional=${p.optional}"
}
// factor: Type.Float optional=false
// xs: Type.List(Type.F32) optional=false
// ns: Type.List(Type.IntN(32, true)) optional=false
// label: Type.String optional=true
```

**Declared types and fixed widths.** `params_of` and `type_of` answer with the *same* `Type` for the same declared type — they share one decoder, so they cannot drift. A runtime scalar carries no width tag, so at **top level** a declared fixed-width scalar erases exactly as its value does: every `iN`/`uN` parameter reflects `Type.Int` and `f64` reflects `Type.Float`, while `f32` is reified and keeps `Type.F32`. In **container-element position** a width is a physically distinct storage slot and is preserved at any depth: `List<i32>` reflects `Type.List(Type.IntN(32, true))`. The practical consequence: matching a signature from `params_of` against runtime values (dependency injection, CLI/router derivation) works for every scalar width — `type_of(5)` is `Type.Int`, and so is an `i32` parameter's `type`. See [Fixed-Width Integers](Fixed-Width-Integers) for the erasure model.

### `attributes_of::<T>(): List<Attributed<T>>`

Materializes every `#[T(...)]` attribute in the program — each entry's `.value` is a real `T`, and `.target` is the annotated declaration's name:

```noeta check
@attribute
struct Route { path: string }

#[Route("/users")]
fn list_users(): string { return "…" }

routes = attributes_of::<Route>()
for r in routes {
    echo "${r.target} -> ${r.value.path}"
}
```

A type-reference argument arrives as a full reflection `Type` value, generic arguments intact — what a codegen or DI consumer needs to reconstruct the declared type, not just its head:

```noeta
@attribute
struct Builds { target: Type }

#[Builds(target: List<int>)]
fn make_list(): List<int> { return [] }

for b in attributes_of::<Builds>() {
    echo "${b.target}: ${b.value.target}"    // make_list: Type.List(Type.Int)
}
```

### `roles_of(): List<RoleBinding>` / `roles_of::<RoleEnum>(): List<RoleBinding>`

The compile-time `(declaration, role)` index built from `@role(...)` tags — each binding has a `.target` and a `.role`. The optional turbofish scopes the query to a single `@semantic` enum (the mirror of `attributes_of::<T>()`): `roles_of::<Semantic>()` returns only the bindings whose role is a `Semantic` variant, while bare `roles_of()` returns the whole index. The enum is resolved at compile time (closed-world); naming a non-`@semantic` type is an error (E0031).

### `invoke(recv, name, args): Result<dyn, dyn>` / `invoke(name, args): Result<dyn, dyn>`

Fallible dispatch by name. With three operands, `recv` is a value (→ an instance method) or a bare type name (→ an associated function):

```noeta
struct Rect {
    w: int
    h: int
    fn new(w: int, h: int): Rect { return Rect { w: w, h: h } }
    fn area(): int { return self.w * self.h }
}

echo match invoke(Rect.new(2, 3), "area", []) {
    Ok(v)  => "area = ${v}",             // area = 6
    Err(e) => "no such method",
}
```

With two, `name` is a **top-level function** — the same string `params_of` takes for a free fn, so reflecting a signature and then calling it round-trips on one name:

```noeta
fn greet(who: string = "stranger"): string { return "hi ${who}" }

echo match invoke("greet", ["ada"]) {
    Ok(v)  => v,                         // hi ada
    Err(e) => "no such function",
}
```

The two-operand form searches the top-level function namespace and nothing else. A type name, a qualified `Type.method`, and a local variable holding a function are each simply not found — reaching a type's methods is what the three-operand form is for, and a function value you already hold you can just call.

Every resolution failure is an `Err`, never an abort: an unknown name, a non-string name, non-list args, and an arity mismatch alike. A parameter with a default may be omitted from `args`, exactly as at a direct call site — the pair to `ParamInfo.optional`. A panic *inside* the invoked body is a normal abort; only the by-name resolution is caught.

## Where this is headed

The reflection manifest — declarations, their `#[…]` attributes, and their `@role`/`@semantic` tags — backs an agentic tooling surface: `noeta mcp` serves this manifest over stdio (roles, attributes, and the architectural graph) to MCP clients. See [Editor & AI Tooling](Editor-and-AI-Tooling) for the tool inventory.
