//! `para.db` — the database layer's **native driver** (aether DB0), the only native piece of the
//! DB stack (the query builder, repository/unit-of-work, and `@sql` tier above it are pure Noeta).
//!
//! One module and one extern type, rooted at `para`:
//!   * `db`         — `db.connect(dsn) -> Connection`; the dsn scheme selects the driver.
//!   * `Connection` — `execute(sql, params) -> int`, `query(sql, params) -> List<Map<string, dyn>>`,
//!     `migrate(dir) -> int`, `seed(dir) -> int`, `close()`; a shared handle over a boxed
//!     [`driver::SqlDriver`].
//!
//! The **swappable-driver seam** is [`driver::SqlDriver`]: SQLite ([`sqlite::SqliteDriver`], behind
//! `ring-sqlite`) and PostgreSQL ([`pg::PostgresDriver`], behind `ring-postgres`) are the two impls;
//! the dsn scheme selects between them and further backends arrive the same way — no change to the
//! Noeta surface, the query builder, the repository, or the `@sql` tier. Like `para-p2p`, this crate
//! is compiled and linked only when a
//! program depends on the `para/db` package, and registered through the fixed native-extension
//! convention ([`NOETA_EXTENSIONS`], re-exported by the package's `native` entry crate).

pub mod command;
pub mod conn;
pub mod driver;
pub mod migrate;
#[cfg(feature = "ring-postgres")]
pub mod pg;
#[cfg(feature = "ring-sqlite")]
pub mod sqlite;
pub mod watch;

use noeta_ext_abi::registry::{ExtModule, ExtType, Extension};

/// The `para.db` extension unit — the `db` module and the `Connection` extern type. `root() ==
/// "para"`, so the module resolves as `para.db` and the type as `para.db.Connection`.
#[derive(Debug, Clone, Copy)]
pub struct ParaDbExtension;

impl Extension for ParaDbExtension {
    fn name(&self) -> &'static str {
        "para.db"
    }
    fn root(&self) -> &'static str {
        "para"
    }
    fn modules(&self) -> &'static [ExtModule] {
        PARA_DB_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        PARA_DB_TYPES
    }
    /// The `ViewSourceExtract` capability the reactive engine's `view.expose` resolves a `Watch`
    /// through (see [`watch::DB_CAPABILITIES`]) — declared on the unit so it is scoped to whatever
    /// registry this extension is assembled into (and resolved via the broker's plural lookup,
    /// beside `para.synced`'s extractor when both are installed).
    fn capabilities(&self) -> &'static [noeta_ext_abi::registry::ExtCapability] {
        watch::DB_CAPABILITIES
    }
    /// `noeta migrate` (para-extraction) — the migration verb travels with the package: a consumer
    /// that trusts this package's commands (`[trust] commands = ["para/db"]`) gets it from the
    /// composed toolchain; nothing db-specific stays in the core CLI.
    fn commands(&self) -> &'static [noeta_ext_abi::ExtCommand] {
        &[crate::command::MIGRATE_COMMAND]
    }
}

/// The fixed native-extension export convention (package-manager Phase 3): the package's native
/// entry crate re-exports this slice; the composed toolchain aggregates every dependency's slice and
/// installs the union into the runtime registry.
pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&ParaDbExtension];

/// The `para.db` modules — just `db` (the connection factory). No ring here: the driver selection
/// is pure Rust, and SQLite itself rides the crate's own `ring-sqlite` feature, not a runtime ring.
const PARA_DB_MODULES: &[ExtModule] = &[ExtModule {
    name: "db",
    functions: crate::conn::DB_FNS,
    dispatch: crate::conn::db_dispatch,
    // `db.watch(conn, channel)` reaches the reactive engine, so it is a higher-order (ctx) function
    // alongside the plain `db.connect` (aether DB5, reactive DB source).
    ctx_functions: crate::watch::WATCH_FNS,
    ctx_dispatch: Some(|func, ctx, args| crate::watch::watch_ctx_dispatch(func, ctx, args)),
    docs: DB_DOCS,
    ..ExtModule::DEFAULTS
}];

/// The `para.db` extern types — the `Connection` handle and the reactive `Watch` source (DB5).
/// `Connection` declares `deep_marshal` so its `List<dyn>` params project to a full `NativeValue`
/// tree the driver can read.
const PARA_DB_TYPES: &[ExtType] = &[
    ExtType {
        name: crate::conn::CONNECTION_TYPE_NAME,
        namespace: "para.db",
        methods: crate::conn::CONNECTION_METHODS,
        dispatch: crate::conn::CONNECTION_DISPATCH,
        deep_marshal: true,
        docs: CONNECTION_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::watch::WATCH_TYPE_NAME,
        namespace: "para.db",
        ctx_methods: crate::watch::WATCH_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::watch::watch_ctx_method_dispatch(method, ctx, recv, args)
        }),
        docs: WATCH_DOCS,
        ..ExtType::DEFAULTS
    },
];

const DB_DOCS: &[(&str, &str)] = &[
    (
        "connect",
        "Open a database connection from a dsn — the scheme selects the driver: `sqlite::memory:` (or \
         `:memory:`) for an in-memory database, `sqlite:PATH` (or a bare path) for a SQLite file, or \
         `postgres://user:pass@host:5432/db` (`postgresql://` too) for a PostgreSQL server. Returns a \
         `Connection`.",
    ),
    (
        "watch",
        "Create a reactive `Watch` over a notification `channel` (Postgres `LISTEN`) — a node in the \
         `std.reactive` graph whose value is a revision counter. A `computed` that reads `watch.get()` \
         and re-queries the database re-runs whenever an external write fires `NOTIFY channel` and the \
         app `pump`s the watch — the basis of keeping a UI in sync with the database.",
    ),
];

const WATCH_DOCS: &[(&str, &str)] = &[
    (
        "get",
        "Read the watch's revision reactively — subscribes the running `computed`/`effect`, so it \
         re-runs when the watch wakes.",
    ),
    (
        "pump",
        "Poll pending change notifications non-blocking; if any fired on this channel, bump the \
         revision and wake every dependent. Returns whether it woke. Call it from the app's loop.",
    ),
];

const CONNECTION_DOCS: &[(&str, &str)] = &[
    (
        "execute",
        "Run a non-query statement (`INSERT`/`UPDATE`/`DELETE`/DDL, or `BEGIN`/`COMMIT`/`ROLLBACK`) \
         with positional `?` bind parameters; returns the number of rows affected.",
    ),
    (
        "query",
        "Run a query with positional `?` bind parameters; returns each result row as a \
         `Map<string, dyn>` of column name to value.",
    ),
    (
        "notify",
        "Fire a change notification on a channel (Postgres `NOTIFY`) — wakes any `db.watch` listening \
         on it (this connection or another). A no-op on a driver without a push channel (SQLite).",
    ),
    (
        "migrate",
        "Apply every pending SQL migration under `dir` (default project layout: `migrations/`), each \
         in its own transaction, and return the number applied (0 when already up to date). Uses the \
         `_noeta_migrations` tracking table and the same checksum/deleted-file integrity checks as \
         `noeta migrate`; call it at boot for a self-migrating app.",
    ),
    (
        "close",
        "Release the connection. The handle is also freed automatically when the last reference to \
         it drops.",
    ),
];
