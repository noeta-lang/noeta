# para/aether-db

**DB-backed, revocable sessions for [para/aether](../para-aether).** The stored upgrade to
`std.session`'s stateless `CookieSessions`.

`std.session` carries the whole session in a signed cookie — no server state, correct under
`serve --parallel`, nothing to configure. That is the right default, and most apps never need more.
Two things it cannot do, by construction:

- **Grow past ~4 KB.** A browser silently drops a cookie over about 4 KB, so a stateless session has
  a hard ceiling. `std.session` errors when you hit it rather than losing state quietly — and that
  error is the signal to move here.
- **Revoke.** A signed cookie is valid until it expires; there is nothing server-side to invalidate,
  so a stolen token is stolen until expiry and a "log out everywhere" button is a lie. A stored
  session's authoritative copy lives in a row, so deleting the row is a **real** logout: a copy of
  the id taken earlier no longer resolves to anything.

`para/aether-db` keeps only a small opaque id in the cookie and the data in a `sessions` row. It
implements para/aether's `SessionStore` over the **same** `std.session` `Session`, so `DbSessions`
and `CookieSessions` are interchangeable at a `dyn SessionStore` call site — a handler written
against one moves to the other without a line changing.

## The opt-in is one manifest line

The store lives in its own companion package, not in para/aether and not in para/db, on purpose:

- **Not in para/db** — a database package must not depend on the web framework.
- **Not in aether-core** — that would make para/db's native SQLite/Postgres footprint mandatory for
  *every* aether app, including the ones happy with cookie sessions.

So an app reaches for stored sessions by adding `para/aether-db` to the `para` scope it already lists
aether (and para/db) under:

```toml
[dependencies]
para = [
    { path = ".../para-aether" },
    { path = ".../para-db" },
    { path = ".../para-aether-db" },   # <- add this
]

[trust]
native = ["para/db"]                    # para/db's native driver; para/aether-db is pure Noeta
```

Discoverability is that dependency plus this doc — an app that references `para.aether_db.DbSessions`
without the package gets an ordinary unresolved-import error. There is deliberately no bespoke
compiler diagnostic: there is no central hook for one, and the import error already names the missing
symbol.

## Use it

**1. Migrate.** The store needs one table. Copy the migration (or point `conn.migrate` at this
package's `migrations/`) — it is plain SQL that runs verbatim on both drivers para/db ships:

```
migrations/20260722120000_create_sessions.sql   # id TEXT PK, data TEXT, expires_at INTEGER
```

```noeta ignore
conn = db.connect("sqlite:app.db")
conn.migrate("migrations")
```

**2. Bind the store** under the `SessionStore` interface, and inject it into handlers:

```noeta ignore
use para.aether.{App, Get, Post, SessionStore}
use para.aether_db.DbSessions
use std.http.server
use std.http.{Request, Response}

app.bind("SessionStore", DbSessions.new(conn, "sid", 3600, true))
//                                    conn ─┘  │      │     └ Secure (true behind https)
//                             cookie name ────┘      └ max-age seconds

class Account {
    #[Post("/login")]
    fn login(req: Request, store: dyn SessionStore): Response {
        s = store.open(req).set("user", "42")
        return store.attach(server.response(200, "hi"), s)
    }

    #[Post("/logout")]
    fn logout(req: Request, store: dyn SessionStore): Response {
        s = store.open(req).clear()                       // empties the session
        return store.attach(server.response(200, "bye"), s)  // → DELETEs the row, expires the cookie
    }
}
```

The runnable end-to-end version — login, a stored round-trip, and a logout that deletes the row — is
[`examples/para-aether/aether-sessions-db`](../../examples/para-aether/aether-sessions-db).

## How it behaves

| Handler does | `attach` does |
|---|---|
| reads only (`open`, no `set`/`remove`/`clear`) | nothing — no row write, no `Set-Cookie` (a read is free) |
| writes (`set`/`remove`) | UPSERTs the row (data as JSON, fresh expiry) under its id — minted on the first write — and sets the id cookie |
| `clear()` | **DELETEs** the row and expires the cookie — a revocable logout |

The cookie is `HttpOnly`, `SameSite=Lax`, `Path=/`, and `Secure` per the `secure` argument (`true`
in production; `false` only for a plain-http localhost — the same out-loud choice `CookieSessions`
asks for, because both wrong answers fail silently). The id is 16 crypto-random bytes (128 bits),
hex-encoded — an unguessable **key**, never a signed payload: the row is the source of truth, so the
id carries no data and needs no verification.

Expiry is compared in-app against `datetime.now()` (a bound parameter), not in SQL, so the store's
one statement runs unchanged on SQLite and PostgreSQL alike.
