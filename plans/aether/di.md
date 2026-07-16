# L2 — Dependency injection (design)

**Goal:** Laravel-style DI — `fn create(body: CreateUser): Response` where the router materializes
`body` from the request JSON by the handler's declared param type. Also: a bound ORM resource
(route-model binding) later.

## Grounding (from the machinery map)

- The typed-decode engine is COMPLETE: `TypeRecipe` (`noeta-native/src/registry.rs:318`) + `parse_typed`/
  `decode`/`materialize_recipe` (both backends). `type_to_recipe` (`check lib.rs:6616`) already builds a
  struct recipe (fields in declared order). `json.parse::<T>` bakes the recipe at the call site.
- **`json.parse::<T>` ABORTS on malformed input** (both backends surface a fatal runtime error, not a
  `Result`). A web framework needs a recoverable decode → 400, not a crash.
- Module-level per-type registries baked into `Module` are established 5×: `methods`, `tojson_derives`,
  `comparable_derives`, `destructors`, `field_defaults` — the precedent for a `deserialize_recipes`
  table keyed by type name.
- `invoke(receiver, name, args)` returns a recoverable `Result<dyn,dyn>`; a reflection `Type` value IS
  a valid invoke receiver for a static fn (`invoke(type_of(x), "new", …)` works when the name is a real
  global). But there is NO signature/param reflection and NO string→type-handle surface.
- `@derive(Serialize<Json>)` → `to_json` is a per-type FLAG + a value-only **instance intercept**
  (`eval lib.rs:2136`, VM `lib.rs:4724`), NOT a method-table entry. `from_json` is *associated*
  (constructs a T) — a different shape; mirroring the intercept is awkward, so we go recipe-driven.

## Design: recipe-driven (research-recommended B), sliced

- **L2.1 — recoverable turbofish decode** `json.decode::<T>(text) -> Result<T, string>`. A Result-
  wrapping sibling of `json.parse::<T>` — reuses the call-site recipe + `parse_typed`, but on `Err`
  builds `Result.Err(msg)` instead of aborting. HANDLER-facing (concrete T). No derive/registry.
  Small; validates the Result-wrapping we reuse in L2.2.
- **L2.2 — `@derive(Deserialize<Json>)` + runtime registry + `json.decode_typed`.** A new derivable
  trait `Deserialize` (mirror `Serialize`: `BuiltinTrait`, `check_derives`, format check, field
  constraint = `type_to_recipe` returns `Some`). The checker records the deriving type's recipe into
  a `deserialize_recipes: Vec<(String, TypeRecipe)>` baked into `Module` (mirror `tojson_derives`),
  lifted to a runtime `HashMap<String,TypeRecipe>` in both backends. `json.decode_typed(t: Type, text)
  -> Result<dyn, string>` extracts `t`'s name, looks up the recipe, `parse_typed`, recoverable. This
  is the ROUTER path (type known only at runtime).
- **L2.3 — parameter-type reflection** `params_of(target) -> List<Param{name, type: Type}>` (and/or
  `signature_of`). Record each method/fn's param (name, resolved `Type`) into the reflection manifest
  (`reflect::build`), keyed like attributes (`"Controller.method"`). The genuine gap — reflection has
  attributes + invoke but no signatures today.
- **L2.4 — framework composition** (in `para/aether`, later): the router reads params via L2.3; for
  each param picks a source — `Request`, a deserializable body (`decode_typed`), or a bound model —
  builds the arg list, and `invoke(controller, method, args)`. Ships when the para package is
  scaffolded (blocked on the para-rework agent).

## Error surface
Decode failures are recoverable `Result.Err(message)` (the `ok_shape`/`err_shape` pattern from
`Op::Invoke`), never a program abort — so the router turns a bad body into a 400.

## Riskiest points
1. Result-wrapping in the TypedModuleCall path needs the Ok/Err enum shapes at the call site (as
   `Op::Invoke` carries them) — thread them onto the decode op.
2. `deserialize_recipes` must be reachable where `decode_typed` executes — bake into `Module`, read in
   the backend's own dispatch (where `self`/the table is available), not via a stateless native.
3. Keep eval + VM byte-identical (differential oracle) for every slice.
