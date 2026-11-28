//! The server's SQLite database: the op log with each op's result, refused ops, the
//! authority's projection, counters, actor tokens, and the protocol version. Schema versions are SQLite's `user_version`; each migration
//! moves one version forward in its own transaction.

use std::fmt;
use std::path::Path;

use rusqlite::Connection;

/// Schema steps: the index `i` step moves `user_version` from `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[
    // v1: the tables PLAN.md names.
    "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
     CREATE TABLE ops (
         seq INTEGER PRIMARY KEY AUTOINCREMENT,
         op_id TEXT NOT NULL UNIQUE,
         actor TEXT NOT NULL,
         at TEXT NOT NULL,
         task_id TEXT,
         body TEXT NOT NULL
     ) STRICT;
     CREATE INDEX ops_task ON ops (task_id);
     CREATE TABLE tasks (id TEXT PRIMARY KEY, body TEXT NOT NULL) STRICT;
     CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL) STRICT;
     CREATE TABLE tokens (
         hash TEXT PRIMARY KEY,
         actor TEXT NOT NULL,
         scope TEXT NOT NULL
     ) STRICT;",
    // v2: every op keeps its result, so a resent op_id gets the original answer (task_id,
    // never written, goes). Accepted
    // ops record the numbers their creates got; refused ones keep their reason, unordered.
    // The authority's state is one projection row, written with the ops it reflects.
    "ALTER TABLE ops ADD COLUMN numbers TEXT NOT NULL DEFAULT '{}';
     DROP INDEX ops_task;
     ALTER TABLE ops DROP COLUMN task_id;
     CREATE TABLE rejections (
         op_id TEXT PRIMARY KEY,
         actor TEXT NOT NULL,
         body TEXT NOT NULL,
         reason TEXT NOT NULL
     ) STRICT;
     DROP TABLE tasks;
     CREATE TABLE projection (
         id INTEGER PRIMARY KEY CHECK (id = 1),
         seq INTEGER NOT NULL,
         state TEXT NOT NULL
     ) STRICT;",
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

#[derive(Debug)]
pub enum DbError {
    Sqlite(rusqlite::Error),
    /// The database was written by a newer `tsk-sync`.
    Newer {
        found: u32,
        supported: u32,
    },
    /// A stored op, result, or projection that does not parse, or one that does not encode.
    Json(serde_json::Error),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "database error: {error}"),
            Self::Json(error) => write!(f, "stored JSON error: {error}"),
            Self::Newer { found, supported } => write!(
                f,
                "database schema {found} is newer than this tsk-sync knows ({supported}): \
                 run a tsk-sync built with schema {found} or later"
            ),
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Open (creating it if needed) and migrate the database at `path`.
pub fn open(path: &Path) -> Result<Connection, DbError> {
    let mut connection = Connection::open(path)?;
    // Refuse a newer database before changing anything about it, journal mode included.
    check_version(&connection)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut connection)?;
    Ok(connection)
}

/// The database's schema version, refusing one newer than this build.
fn check_version(connection: &Connection) -> Result<u32, DbError> {
    let found: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(DbError::Newer {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(found)
}

/// Run every migration past the database's version, refusing a newer database.
pub fn migrate(connection: &mut Connection) -> Result<(), DbError> {
    let found = check_version(connection)?;
    for (index, step) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(step)?;
        transaction.pragma_update(None, "user_version", index as u32 + 1)?;
        transaction.commit()?;
    }
    connection.execute(
        "INSERT INTO meta (key, value) VALUES ('protocol_version', ?1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [super::PROTOCOL_VERSION.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .filter(|name: &String| !name.starts_with("sqlite_"))
            .collect()
    }

    #[test]
    fn a_new_database_gets_every_table_and_the_protocol_version() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        assert_eq!(
            tables(&connection),
            [
                "counters",
                "meta",
                "ops",
                "projection",
                "rejections",
                "tokens"
            ]
        );
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let protocol: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'protocol_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(protocol, super::super::PROTOCOL_VERSION.to_string());
        // Migrating again changes nothing and keeps the data.
        connection
            .execute(
                "INSERT INTO ops (op_id, actor, at, body) VALUES ('a', 'you', 't', '{}')",
                [],
            )
            .unwrap();
        migrate(&mut connection).unwrap();
        let ops: u32 = connection
            .query_row("SELECT count(*) FROM ops", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ops, 1);
    }

    #[test]
    fn op_ids_are_unique_and_a_newer_schema_refuses() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let insert = "INSERT INTO ops (op_id, actor, at, body) VALUES ('same', 'you', 't', '{}')";
        connection.execute(insert, []).unwrap();
        assert!(connection.execute(insert, []).is_err(), "op_id is unique");
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(matches!(
            migrate(&mut connection),
            Err(DbError::Newer { found, .. }) if found == SCHEMA_VERSION + 1
        ));
    }

    /// A newer database is refused before anything about it changes, its journal mode too.
    #[test]
    fn open_refuses_a_newer_database_without_touching_it() {
        let path = std::env::temp_dir().join(format!(
            "tsk-sync-newer-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let newer = Connection::open(&path).unwrap();
        newer
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(newer);
        assert!(matches!(open(&path), Err(DbError::Newer { .. })));
        let mode: String = Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        std::fs::remove_file(&path).unwrap();
    }
}
