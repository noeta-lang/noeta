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
