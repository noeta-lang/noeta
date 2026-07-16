//! The **swappable driver seam** (aether DB0): a backend-agnostic SQL surface every concrete
//! database driver implements. SQLite is the first impl ([`crate::sqlite`]); Postgres/MySQL arrive
//! as further [`SqlDriver`] impls with **no change** to the Noeta surface or the extern type — the
//! `db.connect` dsn scheme is the only place a new driver is wired in.

/// A backend-agnostic scalar crossing the driver boundary — the value kinds SQL columns and bound
/// parameters take. Kept deliberately small (the SQLite storage classes); a richer driver maps its
/// own types onto these. `bytes`/decimal/date land in a later slice with the columnar surface.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Null,
}

/// One result row: `(column name, value)` pairs in the query's column order. A `Map<string, dyn>`
/// on the Noeta side — the simplest row surface (struct mapping arrives in DB2).
pub type Row = Vec<(String, SqlValue)>;

/// The swappable database driver. `Send` so a [`crate::conn::ConnectionBox`]'s `Arc<Mutex<Box<dyn
/// SqlDriver>>>` may cross the executor. **Transactions are ordinary statements** —
/// `execute("BEGIN")` / `execute("COMMIT")` / `execute("ROLLBACK")` — deliberately NOT a borrowed
/// `rusqlite::Transaction` handle (which would borrow the `Connection` and so could never live
/// inside an extern box); the unit-of-work flush (DB2) drives them by name.
pub trait SqlDriver: Send {
    /// Run a non-query statement (`INSERT`/`UPDATE`/`DELETE`/DDL/`BEGIN`/`COMMIT`), returning the
    /// number of rows affected. `Err(message)` on a driver/SQL error.
    fn execute(&mut self, sql: &str, params: &[SqlValue]) -> Result<i64, String>;

    /// Run a query, returning every row as column-name → value pairs. `Err(message)` on error.
    fn query(&mut self, sql: &str, params: &[SqlValue]) -> Result<Vec<Row>, String>;
}
