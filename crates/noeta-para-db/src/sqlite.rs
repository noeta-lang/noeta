//! The SQLite [`SqlDriver`] (aether DB0) — the first concrete driver, wrapping a
//! [`rusqlite::Connection`]. Behind the `ring-sqlite` feature so a build that never touches
//! `para.db` links no SQLite. The [`SqlValue`] ↔ rusqlite mapping lives here and nowhere else.

use rusqlite::Connection;
use rusqlite::types::{Value, ValueRef};

use crate::driver::{Row, SqlDriver, SqlValue};

/// A SQLite-backed [`SqlDriver`] over an owned [`rusqlite::Connection`]. The connection is not
/// cloneable — which is exactly why the extern value ([`crate::conn::ConnectionBox`]) shares it
/// through an `Arc<Mutex<…>>` rather than cloning it.
#[derive(Debug)]
pub struct SqliteDriver {
    conn: Connection,
}

impl SqliteDriver {
    /// Open the in-memory database (`sqlite::memory:` / `:memory:`).
    pub fn open_in_memory() -> Result<SqliteDriver, String> {
        Connection::open_in_memory()
            .map(|conn| SqliteDriver { conn })
            .map_err(|e| e.to_string())
    }

    /// Open (creating if absent) the database at `path` (`sqlite:app.db`).
    pub fn open_path(path: &str) -> Result<SqliteDriver, String> {
        Connection::open(path)
            .map(|conn| SqliteDriver { conn })
            .map_err(|e| e.to_string())
    }
}

impl SqlDriver for SqliteDriver {
    fn execute(&mut self, sql: &str, params: &[SqlValue]) -> Result<i64, String> {
        let bound = to_rusqlite(params);
        self.conn
            .execute(sql, rusqlite::params_from_iter(bound.iter()))
            .map(|affected| affected as i64)
            .map_err(|e| e.to_string())
    }

    fn query(&mut self, sql: &str, params: &[SqlValue]) -> Result<Vec<Row>, String> {
        let bound = to_rusqlite(params);
        let mut stmt = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(bound.iter()))
            .map_err(|e| e.to_string())?;

        let mut out: Vec<Row> = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut record: Row = Vec::with_capacity(columns.len());
            for (i, name) in columns.iter().enumerate() {
                let value = row
                    .get_ref(i)
                    .map(from_value_ref)
                    .map_err(|e| e.to_string())?;
                record.push((name.clone(), value));
            }
            out.push(record);
        }
        Ok(out)
    }
}

/// Marshal the neutral parameters into owned rusqlite values (each implements `ToSql`). A `Bool`
/// binds as SQLite's integer 0/1, its natural storage class.
fn to_rusqlite(params: &[SqlValue]) -> Vec<Value> {
    params
        .iter()
        .map(|p| match p {
            SqlValue::Int(n) => Value::Integer(*n),
            SqlValue::Float(f) => Value::Real(*f),
            SqlValue::Text(s) => Value::Text(s.clone()),
            SqlValue::Bool(b) => Value::Integer(i64::from(*b)),
            SqlValue::Null => Value::Null,
        })
        .collect()
}

/// Read a column value out of a result row. SQLite has no boolean storage class, so a column reads
/// back as `Int`; text/blob decode as UTF-8 (blobs lossily — the row surface is textual in DB0).
fn from_value_ref(value: ValueRef<'_>) -> SqlValue {
    match value {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(n) => SqlValue::Int(n),
        ValueRef::Real(f) => SqlValue::Float(f),
        ValueRef::Text(bytes) => SqlValue::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => SqlValue::Text(String::from_utf8_lossy(bytes).into_owned()),
    }
}
