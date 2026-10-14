use std::{path::Path, time::Duration};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::native::short_hash;

/// Live tools may hold a write lock for a moment; wait instead of failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(connection)
}

pub fn open_read_write(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(connection)
}

pub fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
}

/// Tables that have a column named `column`.
pub fn tables_with_column(connection: &Connection, column: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT m.name FROM sqlite_master m JOIN pragma_table_info(m.name) p
         WHERE m.type = 'table' AND p.name = ?1 ORDER BY m.name",
    )?;
    let rows = statement.query_map([column], |row| row.get(0))?;
    rows.collect()
}

/// Hash of every table, column, type, and NOT NULL flag.
pub fn schema_fingerprint(connection: &Connection) -> rusqlite::Result<String> {
    let mut statement = connection.prepare(
        "SELECT m.name, p.name, p.type, p.[notnull]
         FROM sqlite_master m JOIN pragma_table_info(m.name) p
         WHERE m.type = 'table'
         ORDER BY m.name, p.cid",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(format!(
            "{}:{}:{}:{}",
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?
        ))
    })?;
    Ok(short_hash(&rows.collect::<Result<Vec<_>, _>>()?.join("|")))
}
