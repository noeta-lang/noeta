# para/db

The first-party database layer for Noeta — a native swappable driver plus a pure-Noeta query builder,
repository / unit-of-work, and a typed `@sql` block tier.

- `db.connect(dsn)` → `Connection` — the dsn scheme selects the driver:
  - `sqlite::memory:` / `:memory:` — in-memory SQLite
  - `sqlite:PATH` (or a bare path) — a SQLite file
  - `postgres://user:pass@host:5432/db` (`postgresql://` too) — PostgreSQL
- `Connection.execute(sql, params) -> int` / `query(sql, params) -> List<Map<string, dyn>>` — positional
  `?` bind parameters (rewritten per driver; never string-spliced, so no injection risk).
- `use para.db.query` — a fluent query builder; `use para.db.repo` — repository + unit-of-work
  (stage writes during a request, flush them as one batch); `@sql { … }` — a typed SQL statement with
  `${…}` bound-param holes.

## TLS (PostgreSQL)

The Postgres driver uses a pure-Rust rustls connector (the `ring` crypto provider — no OpenSSL / C
build — and the bundled Mozilla root store, so no system trust store is needed). The connection URL's
`sslmode` parameter selects the behavior, mirroring libpq. Two independent security properties vary:
whether the connection **must** be encrypted, and whether the server's certificate is **authenticated**
(verified against the trust store).

| `sslmode` | Encrypted? | Certificate verified? | Notes |
| --- | --- | --- | --- |
| `disable` | ❌ | — | Always plaintext. Use only over an already-trusted local socket. |
| `prefer` *(default)* | when offered | ✅ (when TLS negotiated) | Try TLS and verify against the bundled roots, else fall back to plaintext. The safe default. |
| `require` | ✅ | ❌ | **Encrypted but NOT authenticated** — libpq parity. See the warning below. |
| `verify-ca` | ✅ | ✅ | Mandatory TLS, certificate verified against the bundled roots. |
| `verify-full` | ✅ | ✅ (incl. hostname) | Mandatory TLS, full certificate verification. The strongest mode. |

```noe
conn = db.connect("postgres://user:pass@host:5432/db?sslmode=require")
```

> **`sslmode=require` is encrypted, not authenticated.** It negotiates TLS (so a passive
> eavesdropper on the wire sees only ciphertext) but does **not** verify the server's certificate —
> so it does **not** defend against an active man-in-the-middle who substitutes their own
> certificate. This matches libpq's `sslmode=require`, and is deliberately distinct from `verify-ca`
> / `verify-full`. Reach for it only when the network path to the server is already trusted (e.g. a
> private link) but the server presents a self-signed or otherwise unverifiable certificate. When the
> server has a real CA-issued certificate, prefer `verify-full`; the default `prefer` already verifies
> whenever TLS is negotiated.

An unrecognized `sslmode` value is a clear error before any connection is attempted.

## Reactive queries — keep the UI in sync with the database

`para.db` integrates with `std.reactive`, so a query can be a **reactive value**: when the data
changes, the query re-runs and every dependent — an `effect`, a `computed`, a LiveView
`view.expose(...)` — updates. Reactivity is **opt-in**: the plain `Repository` stays non-reactive and
zero-overhead; you choose it by using `LiveRepository`.

```noe
use para.db
use para.db.reactive.LiveRepository
use std.reactive.effect

conn = db.connect("sqlite::memory:")           // or postgres://…
users = LiveRepository.new("User", "users", "id", conn)

live = users.all()                             // a reactive query (a computed)
effect(fn() {
    echo "UI: ${live.get().len()} user(s)"     // re-renders whenever `users` changes
})

users.add(User { id: 1, name: "Ada", age: 36 })
users.flush()                                  // commit + notify
users.pump()                                   // deliver notifications → the effect re-runs
```

`LiveRepository` wraps a plain `Repository` with three additions: `all()` returns a reactive query,
`flush()` notifies after committing, and `pump()` (called from your loop, e.g. the serve loop) delivers
pending change notifications and wakes the reactive graph. Under the hood it composes `db.watch(conn,
channel)` — a reactive source node over a change-notification channel — with a plain repository.

**How far a change propagates depends on the driver:**

| a write is seen by a reactive query… | in the same process | across parallel-serve workers (isolate threads of one process) | across separate OS processes |
| --- | --- | --- | --- |
| **SQLite** (per-connection update hook + a process bus) | ✅ | ✅ (only channel-name strings cross — `Send`-safe) | ❌ — SQLite has no server to push |
| **PostgreSQL** (`LISTEN`/`NOTIFY`) | ✅ | ✅ | ✅ |

For SQLite, a write through **any** connection in the process fires its update hook and wakes every
`db.watch` on that table — no trigger or explicit `NOTIFY` needed. For PostgreSQL, a write from another
process wakes the UI when the database `NOTIFY`s the channel: either `conn.notify("<table>")` from each
writer, or a trigger so *any* writer fires it —

```sql
CREATE FUNCTION users_notify() RETURNS trigger AS $$ BEGIN PERFORM pg_notify('users', ''); RETURN NULL; END; $$ LANGUAGE plpgsql;
CREATE TRIGGER users_changed AFTER INSERT OR UPDATE OR DELETE ON users FOR EACH STATEMENT EXECUTE FUNCTION users_notify();
```

See `examples/para-db-demo/` — `reactive_demo.noe` (the manual-signal pattern, any driver),
`live_repo_sqlite_demo.noe`, `live_repo_demo.noe` + `watch_demo.noe` (PostgreSQL, external writes).

## Editor highlighting for `@sql`

**With the official Noeta VS Code extension (v0.9.0+), `@sql` highlights automatically** — it bundles
injection for well-known languages by tier name, and `@sql`'s name is its `text:` language, so nothing
extra is needed.

For **other editors** (or a custom setup), `@sql { … }` bodies highlight as SQL through a **one-rule
TextMate injection grammar** — the standard mechanism for a package that declares a text/expression
tier (see the Noeta VS Code extension's README, "Text tiers and embedded languages"). The core
language grammar stays fixed; this attaches by textual match. This package ships that grammar at
[`editors/sql-tier.tmLanguage.json`](editors/sql-tier.tmLanguage.json); contribute it from a VS Code
extension's `contributes.grammars`:

```jsonc
{
  "scopeName": "inline.noeta.para-db.sql-tier",
  "path": "./sql-tier.tmLanguage.json",
  "injectTo": ["source.noeta"],
  "embeddedLanguages": { "meta.embedded.block.sql": "sql" }
}
```

`${…}` holes inside an `@sql` body are scoped back to Noeta, so they highlight as code (not SQL) — the
same split the compiler makes between the SQL statics and the checked Noeta hole expressions.
