# para/db — database layer (design)

The last aether arc: a native `para/db` package. **NOT active-record** — a repository + service
pattern with a unit-of-work, over a general-purpose query builder, over a **swappable driver**
(SQLite first). Plus a typed `@sql` block tier.

## Architecture (layers, bottom-up)

1. **Driver (swappable).** A `Driver` abstraction so backends are interchangeable. SQLite first
   (native Rust, `rusqlite`), later Postgres/MySQL as separate drivers. The driver is the ONLY native
   piece — everything above is pure Noeta. Native surface (minimal):
   - `db.connect(dsn: string) -> Connection` (dsn picks the driver: `sqlite:app.db`, `:memory:`).
   - `Connection` extern type: `execute(sql, params) -> int` (rows affected), `query(sql, params) ->
     List<Row>`, `begin()/commit()/rollback()` (transactions, for the unit-of-work flush).
   - `Row` extern (or a `Map<string, dyn>`): column → value.
   - The Rust side has a `SqlDriver` trait (execute/query/tx) with a `SqliteDriver` impl; `connect`
     dispatches on the dsn scheme. Swapping a backend = a new impl, no Noeta change.

2. **Query builder (general-purpose, pure Noeta).** A fluent builder that produces `(sql, params)`,
   usable directly OR by the repository. `query.table("users").where("age", ">", min).order("name").
   limit(20).to_sql() -> (string, List<dyn>)`. Covers select/insert/update/delete + where/join/order/
   limit. Backend-agnostic SQL (driver dialects handle differences later).

3. **Repository + unit-of-work (pure Noeta).** A `Repository<T>` over a model type: `find(id) -> ?T`,
   `all() -> List<T>`, `where(...) -> List<T>`, `add(entity)`, `update(entity)`, `remove(entity)`.
   The repo **tracks dirty/new/removed entities during a request** (the unit-of-work) and **flushes
   as a batch at end-of-request** (one transaction: batch INSERT/UPDATE/DELETE) — the advanced feature
   the user asked for. Rows ↔ typed structs via reflection (`params_of`/field reflection +
   `json.decode_typed`-style materialization, or a `@derive` for row mapping). Integrates with aether:
   a service provider registers repositories; DI injects a repo into a handler; the aether
   end-of-request hook flushes the unit-of-work.

4. **`@sql` tier (typed SQL statement).** `@tier(sql, text: "sql", expr: Sql)`-decorated handler in
   para/db: `@sql { SELECT * FROM users WHERE id = ${id} }` desugars to `sql([verbatim statics],
   [hole thunks])` → a `Sql` value (SQL text with `?` placeholders + bound params). Execute via
   `conn.run(stmt)`. The tier gives verbatim SQL bodies + `${}` param holes (safe by construction — a
   hole is a bound parameter, never string-spliced → no injection). Later: compile-time validation of
   the SQL against a schema.

## Slices (each green + committed)

- **DB0 — scaffold + native driver.** `packages/para-db/` (native pkg `para/db`, ns `para.db`) +
  Rust crate: `db.connect`, `Connection.execute`/`query`, `Row`, in-memory SQLite. `Driver` trait +
  `SqliteDriver`. Prove `connect → create table → insert → query` from Noeta.
- **DB1 — query builder** (pure Noeta): fluent select/insert/update/delete → `(sql, params)`.
- **DB2 — repository + unit-of-work** (pure Noeta): typed `Repository`, dirty tracking, batch flush
  in one tx. Row↔struct mapping.
- **DB3 — aether integration**: a `DatabaseProvider` registers repos; DI injects a repo; end-of-
  request flush hook; route-model binding (`fn show(user: User)` loads by id via the repo).
- **DB4 — `@sql` tier**: typed SQL statement values with param holes; `conn.run(@sql{...})`.

## Blueprint confirmations (from the native-ABI audit)
- **Extern box = Pattern A**: `ConnectionBox(Arc<Mutex<Box<dyn SqlDriver>>>)` — `clone_box` = cheap Arc
  clone (solves the non-cloneable `rusqlite::Connection`); plain `ModuleDispatch`/`TypeDispatch`, NO
  `NativeCtx`/ExtState needed. `SqlDriver` trait (the swap seam) lives INSIDE the box.
- **Crate name = `noeta-para-db`** (mirrors `noeta-para-p2p`). NOT `noeta-db` — that's the taken salsa
  pipeline crate. Package `para/db`, ns `para.db`, module `db`, extern type `Connection`.
- **rusqlite + `bundled` builds offline** (verified, ~8s; compiles SQLite C). Gate behind a
  `ring-sqlite` feature (like para-p2p's `ring-p2p`) so an AOT binary sheds it when unused.
- **Transactions**: SQL statements (`BEGIN`/`COMMIT`/`ROLLBACK`) + a flag inside the driver — NOT a
  `rusqlite::Transaction` (borrows the connection; can't live in an extern box). Unit-of-work is pure
  Noeta on top.
- **`@sql` tier = pure Noeta** (option 1): copy `examples/sql_tier.noe` — `@tier(sql, text: "sql",
  expr: Sql) fn sql(statics: List<string>, holes: List<() -> dyn>): Sql`. Confirmed: a `text:` verbatim
  body AND `${}` holes work together; a hole is a bound `?` param (injection-safe by construction). No
  native tier work needed.
- **Local build+test works**: `examples/para-db-demo/` with a path dep + `[trust] native = ["para/db"]`,
  run via `noeta run` (composes the toolchain in-repo, no publish). Native pkgs are NOT conformance-
  differential tested (they need the composed toolchain) — test via example runs + Rust integration
  tests (mirror `crates/noeta-para-p2p/tests/`).
- **Row→struct**: reuse the `TypeRecipe` decode machinery (feed a row Map through it) OR map in Noeta
  via reflection (the aether `json.decode_typed`/`params_of` pattern). Decide in DB2.

## Open questions / risks
- Native package local build+test without publishing (trust gate `[trust] native`) — confirm the
  composed-toolchain path works in-repo (mirror para/p2p's test setup).
- Row→struct materialization: reuse the `TypeRecipe` decode machinery (feed a row Map through it) vs
  a dedicated `@derive(Row)` — decide in DB2.
- The `@sql` tier being BOTH `text:` (verbatim SQL) AND `${}` holes — confirm the tier ABI supports it
  (the `@html` tier is the precedent).
- Determinism/differential: DB is stateful I/O — under the sandbox it needs a deterministic in-memory
  driver (like the sandbox Host for fs/net) so the oracle stays green.
