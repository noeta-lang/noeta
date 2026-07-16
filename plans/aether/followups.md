# noeta-aether — deferred follow-ups (address before the arc closes)

Tracked language/runtime gaps surfaced while building the framework. Revisit each when the
noeta-aether arc completes (before merge).

## F1 — Match arms are expression-only
A `match` arm body with statements/reassignment does NOT parse:
```noe
match r {
    Ok(v) => { x = v }        // E0003 — `{ x = v }` block-with-reassignment is not an expression arm
    Err(e) => { return "..." }
}
```
Arms must be expressions today; framework code works around it with `.map`, expression arms, and
early-return helpers (`arg_for` in the DI router). This is an ergonomic wall a real framework hits
constantly. **Follow-up:** support block/statement bodies in match arms (a general language slice —
lower a block arm to a scoped statement sequence with a tail value). Surfaced during L2.4 DI router.

## F2 — Deserialize recipes don't register in the checkerless REPL `extend` path
`@derive(Deserialize<Json>)` records its `TypeRecipe` via the checker's `type_to_recipe`. The REPL
session's `extend` path (`noeta-compiler` `extend_impl`) lowers checkerless, so a deserializable type
DECLARED IN A REPL ENTRY won't register for `json.decode_typed` (same limitation class as
`tojson_derives` / `comparable_derives`). The checked whole-program path (CLI `run`/`build`, tests,
serve) is fully covered — only interactive REPL redefinition is affected. **Follow-up:** thread a
recipe recompute into the session path (or run a lightweight `type_to_recipe` pass in `extend`) so
REPL-declared deserializable types work. Surfaced during L2.2.

## F3 — VM can't compile a closure inside a method that captures `self` / a field
```noe
fn serve(port: int): void {
    server.serve(port, fn(req) => self.route(req))   // "internal error: the VM cannot compile
                                                       //  this program: a closure inside a method
                                                       //  capturing `self` or a field"
}
```
Worked around in the aether `App.serve` by passing the **bound method** `self.route` directly
(`server.serve(port, self.route)`), which compiles. But any method that needs an inline `self`-capturing
closure (very common — event handlers, callbacks, `map`/`filter` over `self.field`) hits this. **Follow-up:**
teach the VM closure-compilation to capture `self`/fields inside a method body (the eval backend
handles it; this is a VM codegen gap). Surfaced during L2.4.

## F4 (minor) — `Map` has no `.get(k) -> ?V`
Map lookup is `.has(k)` + `m[k]` or `.get_or(k, default)`; there's no `.get(k) -> ?T` returning an
Option (unlike some other collections). A `?T` getter would compose better with `match some/none`.
Minor ergonomics; surfaced during L2.4.

## F5 — a native extern type imported across a package boundary doesn't unify by identity
In `packages/para-db/query.noe` (module `para.db.query`), a param typed `db.Connection` (the native
extern type, via `use para.db`) does NOT accept the `Connection` value a sibling module's
`db.connect` returns — E0007 "argument of type `Connection` is not assignable to `Connection`" (same
short name, two qualified identities). Worked around by typing such params `dyn` (method dispatch
still reaches the native methods) + narrowing the result (`.as<int>()`). **Follow-up:** unify a
native extern type's qualified identity whether it arrives as a runtime value (from a native fn) or
as a `mod.Type` annotation in a consumer/sibling module — the cross-package analogue of the
namespaced-types work, extended to native externs. Surfaced during DB1.

## F6 (minor) — a free fn and a local of the same name don't shadow cleanly
`fn new(table: string) { Q { table: table } }` with a top-level `fn table(...)` in scope resolved the
RHS `table` to the FREE FUNCTION, not the parameter (E0007). Renamed the field/param to avoid it.
A local/param should shadow a same-named global in value position. Surfaced during DB1.
