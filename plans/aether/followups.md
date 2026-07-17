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

> Corollary (DB2): a per-branch SIDE-EFFECT is also blocked in a match arm — `match x { some(u) => echo ..., none => ... }` fails because `echo` is a statement, not just reassignment/return. Display/side-effect code must decompose into helper fns that RETURN a value, called at statement level.

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

## F7 — an app can't use two packages that share a scope — ✅ RESOLVED (scope dependencies)
`para/aether` and `para/db` both have root scope `para`, but a consumer dep key maps to one package
and two `para = ...` entries are a TOML duplicate key — so an app could not depend on both. **Fixed**
with **scope dependencies**: `para = [ { path = … }, { path = … } ]` — an array value binds several
member packages (all sharing one `company` segment) under one import-root key, sharing one global
segment so their literal `para.<pkg>.*` namespaces co-locate in the flat pool (a native member's
`use para.db` retains because the key is the scope). This is the local, forward-compatible form of
the multi-package-per-scope resolution the hosted registry (F4) will later serve. DB3 built on top.

## DB3 — DONE ✅ (route-model binding + service injection + unit-of-work)
Built on F7. aether stays driver-agnostic (its own `Store` interface); the app backs it with para.db
and depends on both via a scope dependency. Two language gaps surfaced and were fixed along the way:
`dyn Trait` reflection (`Type.DynTrait(name)`, for service injection by interface) and cross-module
**standalone** `impl Trait for T {}` linking (a dependency's standalone impl was silently dropped;
only inline impls survived). Both fixed + covered by conformance tests.

## F8 — an Option marshaled to its variant name, not its payload — ✅ RESOLVED
`"7".to_int()` correctly returns `?int` (`some(7)`); binding that Option to a query silently matched
nothing. Root cause: **both backends' `to_native_deep` marshaled *any* enum — including an `Option` —
to its variant *name*** (`some(1)` → the string `"some"`), so it bound the text `'some'` and
`json.stringify(some(1))` produced `"some"`. **Fixed** by marshaling an `Option` through its payload
(`some(x)` → x, `none` → null/unit) in `noeta_value::to_native_deep` and eval's `value_to_native_deep`
(differential-identical). The demo now binds a bare `id.to_int()` with no unwrap. Conformance
+std/json_option_payload.

## para/db PostgreSQL driver — ✅ DONE (swappable-driver seam proven)
A second `SqlDriver` (`pg::PostgresDriver`, behind `ring-postgres`) over the sync `postgres` client.
`db.connect("postgres://…")` runs the same Noeta surface as SQLite — the driver rewrites `?`→`$1,$2,…`
and binds each value to its inferred column type (a `PgVal` `ToSql` adapter; untyped NULL fits any
column). The native entry crate declares `ring-postgres`, so the composed toolchain auto-enables it.
Verified with hermetic unit tests + a live round-trip against real PostgreSQL 16, and a Noeta demo.

## F9 — Postgres TLS — ✅ DONE
`db.connect("postgres://…")` supplies a pure-Rust rustls connector (ring provider, `webpki-roots`).
The dsn's `sslmode` governs use (default `prefer`: try TLS, fall back). rustls **verifies** the cert
against the bundled roots — secure by default (managed PG works); a self-signed dev cert needs
`sslmode=disable` or a trusted cert. Open sub-follow-up: a libpq-style require-without-verify mode.

## F10 — per-driver native ring selection — ✅ DONE
The driver is a **runtime** dsn choice invisible to the static footprint scan, so a `--native` binary
got SQLite (the entry crate's default) and a clear error for `postgres://`. Fixed with a declarative
opt-in: `[native] rings = ["ring-postgres"]` in the app manifest; the AOT build unions these with the
footprint rings. Proven: a `--native` binary built with the ring declared connects to a live PG.

## @sql / embedded-language tier highlighting — ✅ DONE (addon auto-injects by name)
Coloring is **not** in the core grammar (only built-in `doc`→markdown is static). The VS Code
extension (v0.9.0) now **bundles** a second injection grammar (`tier-languages.tmLanguage.json`,
injectTo source.noeta) that auto-lights any tier NAMED after a well-known language — @sql, @html,
@css, @json, @yaml, @xml, @graphql, @markdown, @javascript, @python, @shell, @toml — with `${…}` holes
scoped back to Noeta. A first-party tier's name IS its `text:` language, so @sql/@html just work with
no per-package grammar. (VS Code loads grammars statically → a fixed bundled set, not per-project
generated from `text:`; a tier whose name ≠ language ships its own one-rule grammar — para/db still
ships `packages/para-db/editors/sql-tier.tmLanguage.json` as the fallback/reference.) **What the
injection needs:** `text:` = the language; `expr:` (→ `${…}` holes) = whether to scope holes back.

## `channel` reserved keyword — ✅ FIXED (now contextual)
`channel` was a hard keyword (`channel::<T>(capacity)`), reserving a very common identifier (bit the
reactive ORM). Made **contextual**: lexed as an ordinary identifier, recognized as the constructor
only when followed by `::<T>(…)`. `channel` is free as a name everywhere else. Oracle 632/632.

## Reactive DB↔UI sync — LEVEL 1 + LEVEL 2 ✅ DONE (opt-in)
The ORM's original value prop (keep the UI in sync with the DB), built opt-in (the plain repository
stays non-reactive). **Level 1 (in-process)** — `examples/para-db-demo/reactive_demo.noe`: a repository
query as a `computed` over a DB-revision `signal` the mutation path bumps. **Level 2 (external writes
→ UI without polling)** — a native reactive DB source: `db.watch(conn, channel) -> Watch`, a node in
the shared `std.reactive` graph (via the `ReactiveSource` seam, like para.synced), whose `pump()`
polls Postgres LISTEN/NOTIFY non-blocking and `wake`s the graph. Plus `conn.notify(channel)` and a
`SqlDriver::listen`/`notifications`/`notify` seam (Postgres impl; SQLite default no-op → in-process
degrade). The **opt-in** is `para.db.reactive.LiveRepository` (`packages/para-db/reactive.noe`): wrap a
plain `Repository` to get reactive `all()` + notify-on-`flush` + `pump`. Proven end to end against a
live PG: an EXTERNAL write on a separate connection woke the UI (`examples/para-db-demo/{watch,
live_repo}_demo.noe`, "UI sees 0 → 2"). Driver-agnostic; SQLite degrades to in-process only.

### Language/dev-experience warts surfaced (worth fixing)
- **`channel` is a reserved keyword** (from the coroutine `channel` construct) — it can't be a field
  or variable name, which is surprising for a common identifier (had to rename to `chan`). Consider
  making `channel` a *contextual* keyword so ordinary code can use the name.
- **Compose-cache staleness on a transitive native-source change** — `noeta run`'s composed-toolchain
  cache keys on the entry crate (`para-db-native`), NOT its transitive path-dep impl (`noeta-para-db`),
  so editing the impl crate does not invalidate the cache (stale binary → "no function/method X" until
  `rm -rf ~/.cache/noeta/compose`). The compose key should hash the resolved transitive native source
  (or skip caching for an in-tree path-dep dev build). Bit me repeatedly building DB5.
