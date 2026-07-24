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

## Migrations — evolve the schema over time

`para/db` ships a migration engine, surfaced as the `noeta migrate` command **this package
contributes to the CLI** (trust it with `[trust] commands = ["para/db"]`) and the programmatic
`conn.migrate(dir)` method. There is one engine (in `noeta-para-db`); the command and the Noeta
method are thin callers, so both drivers migrate through the same code.

**Migrations are plain SQL files** in a project `migrations/` directory, one statement or many per
file, applied in the order their filenames sort. `noeta migrate new <name>` scaffolds the next file
with a **UTC-timestamp prefix** — `YYYYMMDDHHMMSS_<name>.sql`. Timestamps are the default because they
never collide when two branches each add a migration (a sequential `0007_…` would); the engine sorts
lexicographically over the whole filename, so any monotonic scheme (including zero-padded sequence
numbers) also works. A migration body is run **verbatim in the target database's native SQL** — there
is no cross-dialect translation, so write portable SQL (a per-dialect `migrations/postgres/` overlay
is a planned option, not in v1).

**A tracking table `_noeta_migrations`** records, for each applied migration, its `filename`, a
**sha256 `checksum`** of the file contents, and `applied_at`. Two integrity checks run before anything
is applied, both hard errors that name the file:

- **Checksum drift** — an already-applied migration's file was edited. History is immutable; revert
  the edit or make the change in a new migration.
- **Deleted applied migration** — a file recorded as applied is gone. Restore it, or `--reset` in
  development.

**Transactionality.** Each migration runs inside its own transaction — `BEGIN`, the file body, the
tracking-row insert, `COMMIT`. The first failure rolls that migration back and stops, reporting the
exact file. Postgres has fully transactional DDL; SQLite is transactional for the ordinary DDL
migrations use — so a migration is all-or-nothing, and a failed run leaves every prior migration
applied. (Do not put `BEGIN`/`COMMIT` in a migration file — the runner owns the transaction.)

**Forward-only.** There are deliberately no down/rollback files: a down migration is routinely wrong
against real production data — the production answer is to roll *forward* with a new migration.
Development uses `--reset` (drop the schema and re-apply from zero), and **seeds** (below) refill the
rebuilt schema with dev data — so `noeta migrate --reset --seed` is the whole development loop. `--reset`
is destructive and driver-specific: on SQLite it drops every user table/view/trigger; on PostgreSQL it
runs `DROP SCHEMA public CASCADE; CREATE SCHEMA public` (the `public` schema only).

## Seeds — re-runnable development data

Where a migration is immutable schema *history*, a **seed** is throwaway development *data*: sample
rows to develop and demo against. Seeds are plain `.sql` files in a project `seeds/` directory,
discovered and ordered by the very same filename-sort convention migrations use, and applied **after**
migrations.

The rerun semantics are deliberately honest and different from migrations. **Seeds run in filename
order, each in its own transaction, every time they are invoked — and are never recorded in the
tracking table.** They are not history, so there is no checksum and no "already applied" skip: running
seeds twice runs every file twice. Idempotency is therefore the seed author's job — write inserts that
a re-run turns into a no-op:

```sql
-- SQLite
INSERT OR IGNORE INTO users (id, name) VALUES (1, 'Ada');
-- PostgreSQL
INSERT INTO users (id, name) VALUES (1, 'Ada') ON CONFLICT DO NOTHING;
```

A mid-seed failure stops at that file (naming it) with the prior seeds committed — the same
stop-on-first-failure shape as migrations, minus the tracking.

### CLI

```
noeta migrate                      # apply every pending migration, printing each applied file
noeta migrate --status             # table of applied / pending migrations
noeta migrate --dry-run            # list what would be applied, without touching the database
noeta migrate new <name>           # scaffold migrations/<timestamp>_<name>.sql
noeta migrate new --seed <name>    # scaffold seeds/<timestamp>_<name>.sql
noeta migrate --seed               # apply pending migrations, then run the seed files
noeta migrate seed                 # run the seed files ONLY (errors if any migration is pending)
noeta migrate --reset --yes        # DESTRUCTIVE: drop the schema and re-apply from zero
noeta migrate --reset --seed --yes # the full dev loop: reset -> migrate -> seed
```

`noeta migrate seed` refuses to run when any migration is still pending — seeding a stale schema is a
footgun; run `noeta migrate` first, or `noeta migrate --seed` to migrate then seed in one step. The
connection string is resolved, highest priority first, from the `--db <dsn>` flag, the `DATABASE_URL`
environment variable, then a `[db]` table in `noeta.toml`:

```toml
[db]
url = "sqlite:app.db"        # or postgres://…  — the same dsn schemes db.connect accepts
migrations = "migrations"    # optional; the directory (default "migrations"), overridable with --dir
seeds = "seeds"              # optional; the directory (default "seeds"), overridable with --seeds-dir
```

`--reset` refuses to run without either `--yes` or an interactive `yes` typed at the prompt.

### At boot (self-migrating apps)

An aether server (or any program) can migrate itself at startup off the same engine, and optionally
seed after — seeding is never implicit, an app opts in explicitly and controls the order (migrate,
then seed):

```noe
conn = db.connect(env.get("DATABASE_URL") ?? "sqlite:app.db")
applied = conn.migrate("migrations")   // returns the count applied; a no-op when up to date
seeded  = conn.seed("seeds")           // runs every seed file every time; returns how many ran
```

`conn.migrate(dir)` applies every pending migration under `dir` and returns how many it applied, with
the same tracking table and integrity checks as the CLI. `conn.seed(dir)` runs every seed file under
`dir` (untracked, re-runnable) and returns how many it ran.

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

See `examples/para-db/para-db-demo/` — `reactive_demo.noe` (the manual-signal pattern, any driver),
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
