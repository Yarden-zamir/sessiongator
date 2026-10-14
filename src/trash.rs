//! Moves a store `Footprint` into a trash directory and back. Files move
//! whole; SQLite rows and JSONL lines are copied into `record.json` and then
//! deleted. The record is written before anything changes, so a failure
//! halfway leaves enough to restore.

use std::{
    fs,
    path::{Path, PathBuf},
};

use gator::AppResult;
use rusqlite::{types::Value as SqlValue, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::stores::{Footprint, LineSet, RowSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trashed {
    pub dir: PathBuf,
    moved: Vec<Moved>,
    rows: Vec<SavedRows>,
    lines: Vec<LineSet>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Moved {
    from: PathBuf,
    to: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedRows {
    set: RowSet,
    columns: Vec<String>,
    rows: Vec<Vec<Cell>>,
}

/// One SQLite value. SQLite has exactly these five storage classes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<SqlValue> for Cell {
    fn from(value: SqlValue) -> Self {
        match value {
            SqlValue::Null => Self::Null,
            SqlValue::Integer(value) => Self::Integer(value),
            SqlValue::Real(value) => Self::Real(value),
            SqlValue::Text(value) => Self::Text(value),
            SqlValue::Blob(value) => Self::Blob(value),
        }
    }
}

impl From<Cell> for SqlValue {
    fn from(cell: Cell) -> Self {
        match cell {
            Cell::Null => Self::Null,
            Cell::Integer(value) => Self::Integer(value),
            Cell::Real(value) => Self::Real(value),
            Cell::Text(value) => Self::Text(value),
            Cell::Blob(value) => Self::Blob(value),
        }
    }
}

/// Remove `footprint` from its store, keeping a restorable copy in `dir`.
pub fn trash(footprint: &Footprint, dir: &Path) -> AppResult<Trashed> {
    fs::create_dir_all(dir)?;
    let files = dir.join("files");
    let moved: Vec<Moved> = footprint
        .paths
        .iter()
        .enumerate()
        .map(|(index, path)| Moved {
            from: path.clone(),
            to: files.join(format!(
                "{index}-{}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("item")
            )),
        })
        .collect();
    let rows = footprint
        .rows
        .iter()
        .map(save_rows)
        .collect::<AppResult<Vec<_>>>()?;
    let trashed = Trashed {
        dir: dir.to_path_buf(),
        moved,
        rows,
        lines: footprint.lines.clone(),
    };
    fs::write(
        dir.join("record.json"),
        serde_json::to_string_pretty(&trashed)?,
    )?;

    for db in databases(&trashed.rows) {
        let mut connection = open_without_foreign_keys(&db)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for saved in trashed.rows.iter().filter(|saved| saved.set.db == db) {
            transaction.execute(
                &format!(
                    "DELETE FROM \"{}\" WHERE \"{}\" = ?1",
                    saved.set.table, saved.set.column
                ),
                [&saved.set.value],
            )?;
        }
        transaction.commit()?;
    }
    for set in &trashed.lines {
        remove_lines(set)?;
    }
    if !trashed.moved.is_empty() {
        fs::create_dir_all(&files)?;
    }
    for item in &trashed.moved {
        move_path(&item.from, &item.to)?;
    }
    Ok(trashed)
}

/// Put a trashed footprint back. Parts that are already back stay as they
/// are, so a restore after a half-done trash works.
pub fn restore(trashed: &Trashed) -> AppResult<()> {
    for item in &trashed.moved {
        if item.to.exists() && !item.from.exists() {
            if let Some(parent) = item.from.parent() {
                fs::create_dir_all(parent)?;
            }
            move_path(&item.to, &item.from)?;
        }
    }
    for set in &trashed.lines {
        restore_lines(set)?;
    }
    for db in databases(&trashed.rows) {
        let mut connection = open_without_foreign_keys(&db)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Parents were deleted last, so they go back first.
        for saved in trashed.rows.iter().rev().filter(|saved| saved.set.db == db) {
            // Some tables have no unique key, so a set that is back already
            // stays as it is instead of getting duplicate rows.
            let present: bool = transaction.query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM \"{}\" WHERE \"{}\" = ?1)",
                    saved.set.table, saved.set.column
                ),
                [&saved.set.value],
                |row| row.get(0),
            )?;
            if present {
                continue;
            }
            let placeholders = vec!["?"; saved.columns.len()].join(", ");
            let columns = saved
                .columns
                .iter()
                .map(|column| format!("\"{column}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT OR IGNORE INTO \"{}\" ({columns}) VALUES ({placeholders})",
                saved.set.table
            );
            for row in &saved.rows {
                let values: Vec<SqlValue> = row.iter().cloned().map(SqlValue::from).collect();
                transaction.execute(&sql, rusqlite::params_from_iter(values))?;
            }
        }
        transaction.commit()?;
    }
    Ok(())
}

fn save_rows(set: &RowSet) -> AppResult<SavedRows> {
    let connection = Connection::open(&set.db)?;
    let mut statement = connection.prepare(&format!(
        "SELECT * FROM \"{}\" WHERE \"{}\" = ?1",
        set.table, set.column
    ))?;
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let rows = statement
        .query_map([&set.value], |row| {
            (0..columns.len())
                .map(|index| row.get::<_, SqlValue>(index).map(Cell::from))
                .collect::<rusqlite::Result<Vec<_>>>()
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(SavedRows {
        set: set.clone(),
        columns,
        rows,
    })
}

fn databases(rows: &[SavedRows]) -> Vec<PathBuf> {
    let mut databases: Vec<PathBuf> = rows
        .iter()
        .filter(|saved| !saved.rows.is_empty())
        .map(|saved| saved.set.db.clone())
        .collect();
    databases.sort();
    databases.dedup();
    databases
}

/// The footprint lists every row of the session, so foreign keys would only
/// cascade into rows that are already in the list, or block the order.
fn open_without_foreign_keys(db: &Path) -> AppResult<Connection> {
    let connection = Connection::open(db)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    Ok(connection)
}

fn remove_lines(set: &LineSet) -> AppResult<()> {
    let content = fs::read_to_string(&set.file)?;
    let mut pending: Vec<&str> = set.lines.iter().map(String::as_str).collect();
    let kept: Vec<&str> = content
        .lines()
        .filter(
            |line| match pending.iter().position(|wanted| wanted == line) {
                Some(index) => {
                    pending.remove(index);
                    false
                }
                None => true,
            },
        )
        .collect();
    let mut output = kept.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    let tmp = set.file.with_extension("sessiongator.tmp");
    fs::write(&tmp, output)?;
    fs::rename(&tmp, &set.file)?;
    Ok(())
}

fn restore_lines(set: &LineSet) -> AppResult<()> {
    let content = fs::read_to_string(&set.file).unwrap_or_default();
    let missing: Vec<String> = set
        .lines
        .iter()
        .filter(|line| !content.lines().any(|existing| existing == line.as_str()))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut output = content;
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    for line in missing {
        output.push_str(&line);
        output.push('\n');
    }
    fs::write(&set.file, output)?;
    Ok(())
}

/// Rename, or copy and delete when `from` and `to` are on different file
/// systems.
fn move_path(from: &Path, to: &Path) -> AppResult<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_recursive(from, to)?;
    if from.is_dir() {
        fs::remove_dir_all(from)?;
    } else {
        fs::remove_file(from)?;
    }
    Ok(())
}

fn copy_recursive(from: &Path, to: &Path) -> AppResult<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        fs::copy(from, to)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stores::tests::temp_dir;

    #[test]
    fn trash_and_restore_roundtrip_files_rows_and_lines() {
        let root = temp_dir("trash");
        let file = root.join("session.jsonl");
        fs::write(&file, "a\n").unwrap();
        let dir = root.join("sidecar");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x"), "x").unwrap();
        let index = root.join("index.jsonl");
        fs::write(&index, "keep\ndrop\nkeep2\n").unwrap();
        let db = root.join("store.db");
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE parent (id TEXT PRIMARY KEY, data BLOB);
                 CREATE TABLE child (session_id TEXT REFERENCES parent(id) ON DELETE CASCADE, n REAL);
                 INSERT INTO parent VALUES ('s', x'00ff'), ('other', NULL);
                 INSERT INTO child VALUES ('s', 1.5), ('s', NULL), ('other', 2);",
            )
            .unwrap();
        let row = |table: &str, column: &str| RowSet {
            db: db.clone(),
            table: table.to_string(),
            column: column.to_string(),
            value: "s".to_string(),
        };
        let footprint = Footprint {
            paths: vec![file.clone(), dir.clone()],
            rows: vec![row("child", "session_id"), row("parent", "id")],
            lines: vec![LineSet {
                file: index.clone(),
                lines: vec!["drop".to_string()],
            }],
        };
        let count = |sql: &str| -> i64 {
            Connection::open(&db)
                .unwrap()
                .query_row(sql, [], |row| row.get(0))
                .unwrap()
        };

        let trashed = trash(&footprint, &root.join("trash")).unwrap();
        assert!(!file.exists() && !dir.exists());
        assert_eq!(fs::read_to_string(&index).unwrap(), "keep\nkeep2\n");
        assert_eq!(count("SELECT count(*) FROM child"), 1);
        assert_eq!(count("SELECT count(*) FROM parent"), 1);

        // A record read back from disk restores the same way.
        let record: Trashed =
            serde_json::from_str(&fs::read_to_string(trashed.dir.join("record.json")).unwrap())
                .unwrap();
        restore(&record).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a\n");
        assert!(dir.join("x").is_file());
        assert!(fs::read_to_string(&index).unwrap().contains("drop"));
        assert_eq!(count("SELECT count(*) FROM child"), 3);
        assert_eq!(count("SELECT count(*) FROM parent WHERE data = x'00ff'"), 1);
        // A second restore changes nothing.
        restore(&record).unwrap();
        assert_eq!(count("SELECT count(*) FROM child"), 3);
    }
}
