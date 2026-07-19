//! The SQLite [`SqlDriver`] (aether DB0) — the first concrete driver, wrapping a
//! [`rusqlite::Connection`]. Behind the `ring-sqlite` feature so a build that never touches
//! `para.db` links no SQLite. The [`SqlValue`] ↔ rusqlite mapping lives here and nowhere else.
//!
//! **Change notifications (DB5).** SQLite is a library, not a server — it has no `LISTEN`/`NOTIFY`.
//! But its per-connection **update hook** fires on every row change through a connection, and the
//! parallel server runs its worker isolates as *threads of one process* (separate heaps, shared
//! address space). So this driver bridges them with a **process-global notification bus** ([`BUS`]):
//! the update hook (and an explicit `notify`) publishes a channel to the bus, and every connection
//! `listen`ing on it — in this isolate or any sibling isolate — sees it on its next `notifications`
//! poll. Only channel-name **strings** cross the bus (no Noeta values, no heaps), so it is
//! `Send`-safe. This gives in-process and cross-*isolate* reactivity; it does NOT reach a separate
//! OS **process** writing the same file (nothing shares the static there) — that is SQLite's hard
//! limit, where Postgres's real LISTEN/NOTIFY is required instead.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};

use rusqlite::Connection;
use rusqlite::types::{Value, ValueRef};

use crate::driver::{Row, SqlDriver, SqlValue};

/// The process-global notification bus: every live [`SqliteDriver`]'s subscriber, weakly held so a
/// dropped connection unregisters itself. A `const`-constructible `Mutex` static (no init needed).
static BUS: Mutex<Vec<Weak<Subscriber>>> = Mutex::new(Vec::new());

/// One connection's bus membership: the channels it `listen`s on, and the channels that have fired on
/// them since the last `notifications` drain. Both `Send + Sync`, so a publish from any isolate thread
/// reaches a subscriber owned by another.
#[derive(Debug, Default)]
struct Subscriber {
    interested: Mutex<HashSet<String>>,
    pending: Mutex<HashSet<String>>,
}

/// Register a fresh subscriber on the bus (and reap any that have since been dropped).
fn bus_register(sub: &Arc<Subscriber>) {
    let mut bus = BUS.lock().expect("notification bus not poisoned");
    bus.retain(|w| w.strong_count() > 0);
    bus.push(Arc::downgrade(sub));
}

/// Publish `channel` to every subscriber `listen`ing on it (this isolate's and every sibling's).
fn bus_publish(channel: &str) {
    let bus = BUS.lock().expect("notification bus not poisoned");
    for weak in bus.iter() {
        if let Some(sub) = weak.upgrade()
            && sub
                .interested
                .lock()
                .expect("subscriber not poisoned")
                .contains(channel)
        {
            sub.pending
                .lock()
                .expect("subscriber not poisoned")
                .insert(channel.to_string());
        }
    }
}

/// A SQLite-backed [`SqlDriver`] over an owned [`rusqlite::Connection`], plus its bus subscriber. The
/// connection is not cloneable — which is exactly why the extern value ([`crate::conn::ConnectionBox`])
/// shares it through an `Arc<Mutex<…>>` rather than cloning it.
#[derive(Debug)]
pub struct SqliteDriver {
    conn: Connection,
    sub: Arc<Subscriber>,
}

impl SqliteDriver {
    /// Wrap an open connection: register a bus subscriber and install the update hook that publishes
    /// a changed table's name (= the channel a `db.watch` on that table listens on) to the bus, so any
    /// write through this connection wakes every watcher on the table — in this isolate or a sibling.
    fn wrap(conn: Connection) -> SqliteDriver {
        let sub = Arc::new(Subscriber::default());
        bus_register(&sub);
        // The hook captures nothing (it calls a free fn over the static bus), so it is `Send + 'static`.
        conn.update_hook(Some(|_action, _db: &str, table: &str, _rowid: i64| {
            bus_publish(table)
        }));
        SqliteDriver { conn, sub }
    }

    /// Open the in-memory database (`sqlite::memory:` / `:memory:`).
    pub fn open_in_memory() -> Result<SqliteDriver, String> {
        Connection::open_in_memory()
            .map(SqliteDriver::wrap)
            .map_err(|e| e.to_string())
    }

    /// Open (creating if absent) the database at `path` (`sqlite:app.db`).
    pub fn open_path(path: &str) -> Result<SqliteDriver, String> {
        Connection::open(path)
            .map(SqliteDriver::wrap)
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

    fn execute_batch(&mut self, sql: &str) -> Result<(), String> {
        // rusqlite's `execute_batch` runs every `;`-separated statement in the script — exactly what a
        // migration file body (and the runner's `BEGIN`/`COMMIT`) needs, and what the single-statement
        // `execute` cannot do.
        self.conn.execute_batch(sql).map_err(|e| e.to_string())
    }

    fn reset(&mut self) -> Result<(), String> {
        // Drop every user object. SQLite has no `DROP SCHEMA`, so enumerate `sqlite_master` and drop
        // each table/view/trigger (an index is dropped with its table; the `sqlite_%` internal objects
        // are left alone). Foreign-key enforcement is turned off for the duration so drop order among
        // referencing tables never matters.
        let objects: Vec<(String, String)> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT type, name FROM sqlite_master \
                     WHERE type IN ('table', 'view', 'trigger') AND name NOT LIKE 'sqlite_%'",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
        };
        let mut script = String::from("PRAGMA foreign_keys = OFF;\n");
        for (kind, name) in objects {
            // `kind` is a fixed sqlite_master token (table/view/trigger); the name is quoted so an
            // arbitrary identifier cannot break out of the statement.
            let quoted = name.replace('"', "\"\"");
            script.push_str(&format!(
                "DROP {} IF EXISTS \"{quoted}\";\n",
                kind.to_uppercase()
            ));
        }
        script.push_str("PRAGMA foreign_keys = ON;\n");
        self.conn.execute_batch(&script).map_err(|e| e.to_string())
    }

    fn listen(&mut self, channel: &str) -> Result<(), String> {
        // Subscribe this connection to `channel` on the process bus. The update hook auto-publishes a
        // changed table's name, and `notify` publishes explicitly; both wake a watcher listening here.
        self.sub
            .interested
            .lock()
            .expect("subscriber not poisoned")
            .insert(channel.to_string());
        Ok(())
    }

    fn notifications(&mut self) -> Result<Vec<String>, String> {
        // Drain the channels that fired since the last poll (non-blocking) — the same contract as
        // Postgres's, so `Watch::pump` is driver-agnostic.
        Ok(self
            .sub
            .pending
            .lock()
            .expect("subscriber not poisoned")
            .drain()
            .collect())
    }

    fn notify(&mut self, channel: &str) -> Result<(), String> {
        // Explicit publish (the write-side companion to `listen`), in addition to the automatic update
        // hook — so a manual `conn.notify(ch)` wakes watchers even without a row change.
        bus_publish(channel);
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    // Unique table/channel names per test — the notification bus is a process-global static shared by
    // all tests (which cargo runs concurrently), so distinct names keep them from cross-firing.

    #[test]
    fn a_write_on_one_connection_wakes_a_listener_on_another() {
        // Two independent connections (separate `:memory:` databases — they share no data, only the
        // process bus). This is the cross-*isolate* case: a write on one wakes a watcher on the other.
        let mut listener = SqliteDriver::open_in_memory().unwrap();
        let mut writer = SqliteDriver::open_in_memory().unwrap();
        listener.listen("t_cross_wake").unwrap();
        assert!(
            listener.notifications().unwrap().is_empty(),
            "nothing before any write"
        );

        writer
            .execute("CREATE TABLE t_cross_wake (id INTEGER)", &[])
            .unwrap(); // DDL does not fire the update hook
        assert!(
            listener.notifications().unwrap().is_empty(),
            "CREATE is not a row change"
        );
        writer
            .execute("INSERT INTO t_cross_wake (id) VALUES (1)", &[])
            .unwrap(); // a row change fires the hook → the bus → the listener

        assert_eq!(
            listener.notifications().unwrap(),
            vec!["t_cross_wake".to_string()]
        );
        assert!(listener.notifications().unwrap().is_empty(), "drained");
    }

    #[test]
    fn a_channel_not_listened_on_is_ignored() {
        let mut a = SqliteDriver::open_in_memory().unwrap();
        a.listen("t_ignore_orders").unwrap();
        let mut b = SqliteDriver::open_in_memory().unwrap();
        b.execute("CREATE TABLE t_ignore_users (id INTEGER)", &[])
            .unwrap();
        b.execute("INSERT INTO t_ignore_users (id) VALUES (1)", &[])
            .unwrap(); // fires the other table
        assert!(a.notifications().unwrap().is_empty());
    }

    #[test]
    fn an_explicit_notify_wakes_a_listener_without_a_write() {
        let mut listener = SqliteDriver::open_in_memory().unwrap();
        listener.listen("t_explicit").unwrap();
        let mut other = SqliteDriver::open_in_memory().unwrap();
        other.notify("t_explicit").unwrap();
        assert_eq!(
            listener.notifications().unwrap(),
            vec!["t_explicit".to_string()]
        );
    }
}
