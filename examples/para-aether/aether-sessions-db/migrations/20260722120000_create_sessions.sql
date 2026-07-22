-- The session store's one table (para/aether-db).
--
-- A row is one live session: an opaque id (the cookie's entire payload — a random, unguessable key,
-- never a signed blob), the session data as a small JSON object, and an absolute expiry in unix
-- seconds. The store filters every read on `expires_at`, so it is indexed.
--
-- Portable DDL: this exact body runs verbatim on both drivers para/db ships (SQLite and PostgreSQL) —
-- `TEXT`, `INTEGER`, and `PRIMARY KEY` mean the same on each. The migration runner owns the
-- transaction, so there is no `BEGIN`/`COMMIT` here.
CREATE TABLE IF NOT EXISTS sessions (
    id         TEXT    PRIMARY KEY,
    data       TEXT    NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_expires_at ON sessions (expires_at);
