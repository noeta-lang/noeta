//! The native surface (aether DB0): the `db` module (`db.connect`) and the `Connection` extern
//! type (`execute` / `query` / `close`), plus the [`ConnectionBox`] extern value that carries a
//! live driver across the ABI.
//!
//! **Pattern A — the critical decision.** A [`rusqlite::Connection`] is not `Clone`, but every
//! extern value must be ([`ExternValue::clone_box`], for GC promotion and argument marshalling).
//! [`ConnectionBox`] resolves this by holding the driver behind an `Arc<Mutex<…>>`: `clone_box` is
//! a **cheap `Arc` clone** (a refcount bump, not a database duplication), so a non-cloneable
//! connection travels the ABI as a shared handle with *reference* semantics — two Noeta bindings to
//! one `Connection` see one database, exactly like `FileHandle`.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;
use std::sync::{Arc, Mutex};

use noeta_ext_abi::registry::{
    ExtFn, NativeOut, NativeValue, RetTy, Scalar, SigType, TypeDispatch,
};
use noeta_ext_abi::{
    ErrorKind, ExternBox, ExternValue, Host, StdError, arity_error, no_function_error, type_error,
};

use crate::driver::{Row, SqlDriver, SqlValue};

/// The registered type's short name — its qualified identity is [`CONNECTION_TYPE_IDENTITY`].
pub const CONNECTION_TYPE_NAME: &str = "Connection";

/// `Connection`'s qualified runtime identity — registered under `para.db`. What
/// [`ExternValue::type_identity`] returns (one pre-joined literal, per the extern-identity
/// contract): `type_of`, `is`/`.as<T>()` narrowing, and method-table lookup key.
pub const CONNECTION_TYPE_IDENTITY: &str = "para.db.Connection";

// --- The extern value (Pattern A) ---------------------------------------------------------------

/// A live database connection as a first-class Noeta value: a shared handle over a boxed
/// [`SqlDriver`]. `Arc` makes `clone_box` cheap (so a non-cloneable driver is legal across the
/// ABI); `Mutex` makes the shared handle `Sync` and serializes concurrent access from isolates.
#[derive(Clone)]
pub struct ConnectionBox(pub Arc<Mutex<Box<dyn SqlDriver>>>);

impl fmt::Debug for ConnectionBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<Connection>")
    }
}

impl ExternValue for ConnectionBox {
    fn type_identity(&self) -> &'static str {
        CONNECTION_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        // Identity, not content: two handles are equal iff they share one driver (`Arc::ptr_eq`).
        other
            .as_any()
            .downcast_ref::<ConnectionBox>()
            .is_some_and(|o| Arc::ptr_eq(&self.0, &o.0))
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        // Pointer identity — not key-capable, but a stable per-handle value.
        Arc::as_ptr(&self.0) as usize as u64
    }
    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "<Connection>")
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        // Cheap: an `Arc` refcount bump, NOT a database duplication — the whole point of Pattern A.
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// --- The `db` module: `db.connect(dsn) -> Connection` -------------------------------------------

const CONNECTION_SIG: SigType = SigType::Named(CONNECTION_TYPE_NAME);

/// The `db` module's function signatures.
pub const DB_FNS: &[ExtFn] = &[ExtFn {
    name: "connect",
    params: &[SigType::String],
    ret: RetTy::Concrete(CONNECTION_SIG),
}];

/// The `db` module dispatch — parses the dsn scheme (the driver-selection point) and returns a
/// boxed `Connection`.
pub fn db_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "connect" => {
            want_arity(func, args, 1)?;
            let dsn = want_str(func, args, 0)?;
            let driver = open_driver(dsn).map_err(io_error)?;
            let conn = ConnectionBox(Arc::new(Mutex::new(driver)));
            Ok(NativeOut::Extern(ExternBox::new(conn)))
        }
        _ => Err(no_function_error("db", func)),
    }
}

/// Parse a dsn and build the driver its **scheme** selects — the one place a new backend is wired in
/// (the swappable-driver seam, DB0). `postgres://…` / `postgresql://…` → the PostgreSQL driver
/// (`ring-postgres`); `sqlite::memory:` / `:memory:` → in-memory SQLite; `sqlite:PATH` or a bare path
/// → a SQLite file (`ring-sqlite`). A scheme whose driver feature is off is a clear error.
pub fn open_driver(dsn: &str) -> Result<Box<dyn SqlDriver>, String> {
    if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        return open_postgres(dsn);
    }
    if let Some(rest) = dsn.strip_prefix("sqlite:") {
        // "sqlite::memory:" → rest == ":memory:"; "sqlite:app.db" → rest == "app.db".
        let path = (rest != ":memory:" && !rest.is_empty()).then_some(rest);
        return open_sqlite(path);
    }
    if dsn == ":memory:" {
        return open_sqlite(None);
    }
    if let Some((scheme, _)) = dsn.split_once("://") {
        return Err(format!(
            "para.db: unsupported driver scheme `{scheme}` in dsn `{dsn}`"
        ));
    }
    // A bare relative/absolute path is a SQLite file.
    open_sqlite(Some(dsn))
}

/// Open a SQLite driver: `None` → in-memory, `Some(path)` → a file.
#[cfg(feature = "ring-sqlite")]
fn open_sqlite(path: Option<&str>) -> Result<Box<dyn SqlDriver>, String> {
    use crate::sqlite::SqliteDriver;
    let driver = match path {
        None => SqliteDriver::open_in_memory()?,
        Some(p) => SqliteDriver::open_path(p)?,
    };
    Ok(Box::new(driver))
}

#[cfg(not(feature = "ring-sqlite"))]
fn open_sqlite(_path: Option<&str>) -> Result<Box<dyn SqlDriver>, String> {
    Err("para.db: this build has no SQLite driver (the `ring-sqlite` feature is off)".to_string())
}

/// Connect a PostgreSQL driver to `dsn` (a `postgres://` / `postgresql://` URL).
#[cfg(feature = "ring-postgres")]
fn open_postgres(dsn: &str) -> Result<Box<dyn SqlDriver>, String> {
    Ok(Box::new(crate::pg::PostgresDriver::connect(dsn)?))
}

#[cfg(not(feature = "ring-postgres"))]
fn open_postgres(dsn: &str) -> Result<Box<dyn SqlDriver>, String> {
    Err(format!(
        "para.db: a `postgres://` dsn needs the `ring-postgres` driver, which this build does not \
         include (dsn `{dsn}`)"
    ))
}

// --- The `Connection` extern type: execute / query / close --------------------------------------

/// The `Connection` type's method signatures. `params` is `List<dyn>` (heterogeneous bound values);
/// `query` returns `List<Map<string, dyn>>` — a row is a column-name → value map (struct mapping is
/// DB2). The type declares `deep_marshal` so these `List`/`Map` arguments arrive fully projected.
pub const CONNECTION_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "execute",
        params: &[SigType::String, SigType::List(&SigType::Dyn)],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        name: "query",
        params: &[SigType::String, SigType::List(&SigType::Dyn)],
        ret: RetTy::Concrete(SigType::List(&SigType::Map(
            &SigType::String,
            &SigType::Dyn,
        ))),
    },
    ExtFn {
        name: "notify",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "migrate",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        name: "seed",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        name: "close",
        params: &[],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// The `Connection` method dispatch entry (paired with [`CONNECTION_METHODS`] at registration).
pub const CONNECTION_DISPATCH: TypeDispatch = connection_method_dispatch;

fn connection_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match method {
        "execute" => {
            want_arity(method, args, 2)?;
            let sql = want_str(method, args, 0)?.to_string();
            let params = want_params(method, args, 1)?;
            let conn = conn_of(recv)?;
            let mut driver = conn
                .0
                .lock()
                .map_err(|_| io_error("connection lock poisoned"))?;
            let affected = driver.execute(&sql, &params).map_err(io_error)?;
            Ok(NativeOut::Scalar(Scalar::Int(affected)))
        }
        "query" => {
            want_arity(method, args, 2)?;
            let sql = want_str(method, args, 0)?.to_string();
            let params = want_params(method, args, 1)?;
            let conn = conn_of(recv)?;
            let mut driver = conn
                .0
                .lock()
                .map_err(|_| io_error("connection lock poisoned"))?;
            let rows = driver.query(&sql, &params).map_err(io_error)?;
            Ok(NativeOut::List(rows.into_iter().map(row_to_out).collect()))
        }
        "notify" => {
            want_arity(method, args, 1)?;
            let channel = want_str(method, args, 0)?.to_string();
            let conn = conn_of(recv)?;
            let mut driver = conn
                .0
                .lock()
                .map_err(|_| io_error("connection lock poisoned"))?;
            driver.notify(&channel).map_err(io_error)?;
            Ok(NativeOut::Unit)
        }
        "migrate" => {
            want_arity(method, args, 1)?;
            let dir = want_str(method, args, 0)?.to_string();
            // Discover + checksum the migration files before locking the driver (a filesystem read
            // over the real project directory, like the SQLite driver opening its file directly).
            let migrations =
                crate::migrate::load_dir(std::path::Path::new(&dir)).map_err(migrate_error)?;
            let conn = conn_of(recv)?;
            let mut driver = conn
                .0
                .lock()
                .map_err(|_| io_error("connection lock poisoned"))?;
            // Apply pending migrations through the shared engine and report how many ran (0 = already
            // up to date), so an app can `conn.migrate("migrations")` at boot.
            let applied =
                crate::migrate::apply(&mut **driver, &migrations).map_err(migrate_error)?;
            Ok(NativeOut::Scalar(Scalar::Int(applied.len() as i64)))
        }
        "seed" => {
            want_arity(method, args, 1)?;
            let dir = want_str(method, args, 0)?.to_string();
            // Discover + order the seed files with the same loader migrations use (a real-filesystem
            // read over the project's `seeds/` directory), before locking the driver.
            let seeds =
                crate::migrate::load_dir(std::path::Path::new(&dir)).map_err(migrate_error)?;
            let conn = conn_of(recv)?;
            let mut driver = conn
                .0
                .lock()
                .map_err(|_| io_error("connection lock poisoned"))?;
            // Run every seed (untracked, re-runnable) through the shared engine and report how many
            // ran, so an app can `conn.seed("seeds")` at boot after `conn.migrate(...)`. Seeding is
            // never implicit — the app opts in and controls the order (migrate, then seed).
            let ran = crate::migrate::seed(&mut **driver, &seeds).map_err(migrate_error)?;
            Ok(NativeOut::Scalar(Scalar::Int(ran.len() as i64)))
        }
        "close" => {
            want_arity(method, args, 0)?;
            // Explicit close discipline (the `FileHandle` convention): DB0 holds no eagerly-freed
            // buffers, so the driver is released when the last `Arc` handle drops. A future driver
            // that must flush/disconnect deterministically does it here.
            Ok(NativeOut::Unit)
        }
        _ => Err(noeta_ext_abi::no_method_error(CONNECTION_TYPE_NAME, method)),
    }
}

// --- Marshalling helpers ------------------------------------------------------------------------

/// Downcast a method receiver to its concrete [`ConnectionBox`].
fn conn_of(recv: &mut dyn ExternValue) -> Result<&ConnectionBox, StdError> {
    recv.as_any()
        .downcast_ref::<ConnectionBox>()
        .ok_or_else(|| type_error("method", CONNECTION_TYPE_NAME))
}

/// Marshal a `List<dyn>` argument into bound [`SqlValue`]s. Requires the type's `deep_marshal`
/// (else the list projects to an `Opaque` the driver cannot read).
fn want_params(
    method: &str,
    args: &[NativeValue],
    index: usize,
) -> Result<Vec<SqlValue>, StdError> {
    match args.get(index) {
        Some(NativeValue::List(elems)) => elems.iter().map(sql_value_of).collect(),
        _ => Err(type_error(method, "List<dyn>")),
    }
}

/// One bound parameter: a scalar/string/unit projects onto a [`SqlValue`]; anything richer (a
/// nested list, a map, an extern) is not a bindable column value in DB0.
fn sql_value_of(value: &NativeValue) -> Result<SqlValue, StdError> {
    match value {
        NativeValue::Scalar(Scalar::Int(n)) => Ok(SqlValue::Int(*n)),
        NativeValue::Scalar(Scalar::Float(f)) => Ok(SqlValue::Float(*f)),
        NativeValue::Scalar(Scalar::F32(f)) => Ok(SqlValue::Float(f64::from(*f))),
        NativeValue::Scalar(Scalar::Bool(b)) => Ok(SqlValue::Bool(*b)),
        NativeValue::Str(s) => Ok(SqlValue::Text(s.clone())),
        NativeValue::Unit => Ok(SqlValue::Null),
        _ => Err(type_error(
            "execute",
            "a scalar, string, or null bind parameter",
        )),
    }
}

/// A result row → a `Map<string, dyn>` output.
fn row_to_out(row: Row) -> NativeOut {
    NativeOut::Map(
        row.into_iter()
            .map(|(name, value)| (name, out_of(value)))
            .collect(),
    )
}

/// A column value → its `NativeOut`. `Null` surfaces as `none`.
fn out_of(value: SqlValue) -> NativeOut {
    match value {
        SqlValue::Int(n) => NativeOut::Scalar(Scalar::Int(n)),
        SqlValue::Float(f) => NativeOut::Scalar(Scalar::Float(f)),
        SqlValue::Text(s) => NativeOut::Str(s),
        SqlValue::Bool(b) => NativeOut::Scalar(Scalar::Bool(b)),
        SqlValue::Null => NativeOut::None,
    }
}

fn want_arity(method: &str, args: &[NativeValue], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(arity_error(method, expected, args.len()))
    }
}

fn want_str<'a>(method: &str, args: &'a [NativeValue], index: usize) -> Result<&'a str, StdError> {
    match args.get(index) {
        Some(NativeValue::Str(s)) => Ok(s),
        _ => Err(type_error(method, "string")),
    }
}

/// A driver/IO failure as the language's IO error.
fn io_error(message: impl Into<String>) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: message.into(),
    }
}

/// A migration-engine failure surfaced as the language's IO error, with the engine's file-naming
/// message preserved.
fn migrate_error(err: crate::migrate::MigrateError) -> StdError {
    io_error(err.to_string())
}
