use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::model::{Session, Tool};
use crate::sources::{SessionSource, Turn};

/// Reads opencode sessions from its SQLite database, strictly read-only.
/// The database uses WAL mode and may be written concurrently by a live
/// opencode process; reads are safe.
pub struct OpencodeSource {
    db_path: PathBuf,
}

/// The table generation that holds a session. OpenCode 1.x writes `session`,
/// `message` and `part`. OpenCode 2 writes `session_v2` and `session_message`.
/// Its first start copies every 1.x session into the 2 tables once and keeps
/// the 1.x tables, so one id can exist in both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    V1,
    V2,
}

impl Store {
    pub(crate) fn session_table(self) -> &'static str {
        match self {
            Store::V1 => "session",
            Store::V2 => "session_v2",
        }
    }

    /// One row per session; `?1` NULL lists all sessions, else only that id.
    /// Columns: id, parent_id, directory, title, agent, model, time_created,
    /// updated, message count. OpenCode 2 does not bump
    /// `session_v2.time_updated` for each message, so "updated" also takes
    /// the newest `session_message` row.
    fn list_sql(self) -> &'static str {
        match self {
            Store::V1 => {
                "SELECT s.id, s.parent_id, s.directory, s.title, s.agent, s.model,
                        s.time_created, s.time_updated,
                        (SELECT count(*) FROM message m WHERE m.session_id = s.id)
                 FROM session s
                 WHERE ?1 IS NULL OR s.id = ?1"
            }
            Store::V2 => {
                "SELECT s.id, s.parent_id, s.directory, s.title, s.agent, s.model,
                        s.time_created,
                        max(s.time_updated, coalesce(
                            (SELECT max(m.time_updated) FROM session_message m WHERE m.session_id = s.id), 0)),
                        (SELECT count(*) FROM session_message m
                         WHERE m.session_id = s.id AND m.type IN ('user', 'assistant'))
                 FROM session_v2 s
                 WHERE ?1 IS NULL OR s.id = ?1"
            }
        }
    }
}

impl OpencodeSource {
    pub fn from_env() -> Self {
        let db_path = std::env::var("OPENCODE_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let base = std::env::var("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| {
                        let home = std::env::var("HOME").unwrap_or_default();
                        Path::new(&home).join(".local").join("share")
                    });
                base.join("opencode").join("opencode.db")
            });
        Self::new(db_path)
    }

    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn connect(&self) -> Result<Connection, String> {
        let connection = Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(3))
            .map_err(|error| error.to_string())?;
        Ok(connection)
    }
}

fn list_store(
    connection: &Connection,
    store: Store,
    id: Option<&str>,
    source_ref: &str,
) -> Result<Vec<Session>, String> {
    let mut statement = connection
        .prepare(store.list_sql())
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([id], |row| {
            let parent_id: Option<String> = row.get(1)?;
            let agent: Option<String> = row.get(4)?;
            let model_raw: Option<String> = row.get(5)?;
            let mut extras = Vec::new();
            if let Some(agent) = agent.filter(|value| !value.is_empty()) {
                extras.push(("agent".to_string(), agent));
            }
            if let Some(parent) = parent_id.filter(|value| !value.is_empty()) {
                extras.push(("parent".to_string(), parent));
            }
            Ok(Session {
                tool: Tool::Opencode,
                id: row.get(0)?,
                title: row
                    .get::<_, Option<String>>(3)?
                    .map(|value| crate::model::clean_title(&value))
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "(no title)".to_string()),
                cwd: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                created_ms: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                updated_ms: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                message_count: row.get::<_, Option<i64>>(8)?.unwrap_or(0) as u32,
                model: model_raw.as_deref().and_then(model_name),
                source_ref: source_ref.to_string(),
                extras,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

/// The store with the newer copy of a session; a tie reads as V2, the
/// generation every current OpenCode writes. Same rule as Collie
/// `bridge/journal/opencode.ts` `sessionStore()`.
pub(crate) fn newest_store(connection: &Connection, id: &str) -> Result<Option<Store>, String> {
    let tables = table_names(connection)?;
    let mut newest: Option<(Store, i64)> = None;
    for store in [Store::V1, Store::V2] {
        if !tables.contains(store.session_table()) {
            continue;
        }
        for session in list_store(connection, store, Some(id), "")? {
            if newest.is_none_or(|(_, updated)| session.updated_ms >= updated) {
                newest = Some((store, session.updated_ms));
            }
        }
    }
    Ok(newest.map(|(store, _)| store))
}

fn table_names(connection: &Connection) -> Result<HashSet<String>, String> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|error| error.to_string())
}

impl SessionSource for OpencodeSource {
    fn tool(&self) -> Tool {
        Tool::Opencode
    }

    fn available(&self) -> bool {
        self.db_path.is_file()
    }

    fn list(&self) -> Result<Vec<Session>, String> {
        let connection = self.connect()?;
        let tables = table_names(&connection)?;
        let source_ref = self.db_path.display().to_string();
        // V2 runs second so a tie keeps the V2 copy (see `newest_store`).
        let mut by_id: HashMap<String, Session> = HashMap::new();
        for store in [Store::V1, Store::V2] {
            if !tables.contains(store.session_table()) {
                continue;
            }
            for session in list_store(&connection, store, None, &source_ref)? {
                let newer = by_id
                    .get(&session.id)
                    .is_none_or(|kept| session.updated_ms >= kept.updated_ms);
                if newer {
                    by_id.insert(session.id.clone(), session);
                }
            }
        }
        let mut sessions: Vec<Session> = by_id.into_values().collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
        Ok(sessions)
    }

    fn transcript(&self, id: &str) -> Result<Vec<Turn>, String> {
        let connection = self.connect()?;
        match newest_store(&connection, id)? {
            Some(Store::V1) => transcript_v1(&connection, id),
            Some(Store::V2) => transcript_v2(&connection, id),
            None => Ok(Vec::new()),
        }
    }
}

fn transcript_v1(connection: &Connection, id: &str) -> Result<Vec<Turn>, String> {
    let mut statement = connection
        .prepare(
            "SELECT m.data, p.data
             FROM part p JOIN message m ON p.message_id = m.id
             WHERE p.session_id = ?1
             ORDER BY p.time_created, p.id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;

    let mut turns = Vec::new();
    for row in rows.flatten() {
        let (message_data, part_data) = row;
        // JSON is parsed here, not with SQLite json_extract, so the query
        // works on any sqlite build.
        let Ok(part) = serde_json::from_str::<Value>(&part_data) else {
            continue;
        };
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = non_empty_text(&part) else {
            continue;
        };
        let role = serde_json::from_str::<Value>(&message_data)
            .ok()
            .and_then(|message| {
                message
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "?".to_string());
        turns.push(Turn { role, text });
    }
    Ok(turns)
}

/// OpenCode 2 keeps one row per message: a user row has `text`, an assistant
/// row has `content` items; only `text` items are transcript text.
fn transcript_v2(connection: &Connection, id: &str) -> Result<Vec<Turn>, String> {
    let mut statement = connection
        .prepare(
            "SELECT type, data FROM session_message
             WHERE session_id = ?1 AND type IN ('user', 'assistant')
             ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;

    let mut turns = Vec::new();
    for row in rows.flatten() {
        let (role, data) = row;
        let Ok(data) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let texts: Vec<String> = if role == "user" {
            non_empty_text(&data).into_iter().collect()
        } else {
            data.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(non_empty_text)
                .collect()
        };
        turns.extend(texts.into_iter().map(|text| Turn {
            role: role.clone(),
            text,
        }));
    }
    Ok(turns)
}

fn non_empty_text(value: &Value) -> Option<String> {
    value
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The session `model` column holds JSON like `{"id":"gpt-5.5","providerID":"openai"}`.
fn model_name(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value
            .get("id")
            .or_else(|| value.get("modelID"))
            .and_then(Value::as_str)
            .map(str::to_string),
        Err(_) => Some(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_SCHEMA: &str = r#"
        CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
            directory TEXT, title TEXT, agent TEXT, model TEXT,
            time_created INTEGER, time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT);
    "#;

    /// The columns sessiongator reads from an OpenCode 2.0.18 database.
    const V2_SCHEMA: &str = r#"
        CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
            parent_id TEXT, directory TEXT NOT NULL, title TEXT, agent TEXT, model TEXT,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
        CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL
            REFERENCES session_v2(id) ON DELETE CASCADE, type TEXT NOT NULL,
            seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, data TEXT NOT NULL);
    "#;

    fn db_with(name: &str, sql: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sessiongator-opencode-{name}-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Connection::open(&path).unwrap().execute_batch(sql).unwrap();
        path
    }

    fn fixture_db(name: &str) -> PathBuf {
        db_with(
            name,
            &format!(
                "{V1_SCHEMA}{}",
                r#"
                INSERT INTO session VALUES ('ses_demo', 'proj', NULL,
                    '/Users/me/Github/demo', 'Investigate flaky test', 'build',
                    '{"id":"gpt-5.5","providerID":"openai"}', 1782755344793, 1782913223968);
                INSERT INTO message VALUES ('msg1', 'ses_demo', 1, 1, '{"role":"user"}');
                INSERT INTO message VALUES ('msg2', 'ses_demo', 2, 2, '{"role":"assistant"}');
                INSERT INTO part VALUES ('p1', 'msg1', 'ses_demo', 1, 1,
                    '{"type":"text","text":"why is the test flaky"}');
                INSERT INTO part VALUES ('p2', 'msg2', 'ses_demo', 2, 2,
                    '{"type":"reasoning","text":"hidden"}');
                INSERT INTO part VALUES ('p3', 'msg2', 'ses_demo', 3, 3,
                    '{"type":"text","text":"It was a race condition"}');
                "#
            ),
        )
    }

    /// `ses_copied` exists in both stores (the V2 migration copied it);
    /// `ses_new` exists only in V2. `v1_updated` sets the V1 copy's time.
    fn migrated_db(name: &str, v1_updated: i64) -> PathBuf {
        db_with(
            name,
            &format!(
                "{V1_SCHEMA}{V2_SCHEMA}
                INSERT INTO session VALUES ('ses_copied', 'proj', NULL, '/w/old',
                    'Old title', 'build', NULL, 100, {v1_updated});
                INSERT INTO message VALUES ('m1', 'ses_copied', 100, 100, '{{\"role\":\"user\"}}');
                INSERT INTO part VALUES ('p1', 'm1', 'ses_copied', 100, 100,
                    '{{\"type\":\"text\",\"text\":\"from v1\"}}');
                {}",
                r#"
                INSERT INTO session_v2 VALUES ('ses_copied', 'proj', NULL, '/w/old',
                    'Old title', 'build', NULL, 100, 200);
                INSERT INTO session_message VALUES ('msg_a', 'ses_copied', 'user', 0, 100, 200,
                    '{"text":"from v2","time":{"created":100}}');
                INSERT INTO session_v2 VALUES ('ses_new', 'proj', NULL, '/w/new',
                    NULL, 'build', '{"id":"gpt-5.5","providerID":"openai"}', 300, 300);
                INSERT INTO session_message VALUES ('msg_b', 'ses_new', 'user', 0, 300, 300,
                    '{"text":"run the tests","time":{"created":300}}');
                INSERT INTO session_message VALUES ('msg_c', 'ses_new', 'system', 1, 301, 301,
                    '{"text":"not transcript","time":{"created":301}}');
                INSERT INTO session_message VALUES ('msg_d', 'ses_new', 'assistant', 2, 302, 900,
                    '{"content":[{"type":"reasoning","text":"hidden"},{"type":"text","text":"All green"},{"type":"tool","id":"t1","name":"shell"},{"type":"text","text":"Done"}]}');
                "#
            ),
        )
    }

    #[test]
    fn lists_sessions_with_parsed_model() {
        let source = OpencodeSource::new(fixture_db("list"));
        let sessions = source.list().unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.id, "ses_demo");
        assert_eq!(session.title, "Investigate flaky test");
        assert_eq!(session.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(session.message_count, 2);
        assert_eq!(
            session.extras,
            vec![("agent".to_string(), "build".to_string())]
        );
    }

    #[test]
    fn transcript_is_text_parts_only_in_order() {
        let source = OpencodeSource::new(fixture_db("transcript"));
        let turns = source.transcript("ses_demo").unwrap();
        assert_eq!(
            turns,
            vec![
                Turn {
                    role: "user".to_string(),
                    text: "why is the test flaky".to_string()
                },
                Turn {
                    role: "assistant".to_string(),
                    text: "It was a race condition".to_string()
                },
            ]
        );
    }

    #[test]
    fn lists_v2_sessions_once_with_newest_message_time() {
        let source = OpencodeSource::new(migrated_db("v2-list", 150));
        let sessions = source.list().unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["ses_new", "ses_copied"]);
        let new = &sessions[0];
        assert_eq!(new.updated_ms, 900);
        assert_eq!(new.message_count, 2);
        assert_eq!(new.title, "(no title)");
        assert_eq!(new.model.as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn transcript_reads_v2_text_in_seq_order() {
        let source = OpencodeSource::new(migrated_db("v2-transcript", 150));
        let turns = source.transcript("ses_new").unwrap();
        let texts: Vec<(&str, &str)> = turns
            .iter()
            .map(|turn| (turn.role.as_str(), turn.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            vec![
                ("user", "run the tests"),
                ("assistant", "All green"),
                ("assistant", "Done"),
            ]
        );
    }

    #[test]
    fn a_session_in_both_stores_reads_the_newer_copy() {
        let tie_or_v2_newer = OpencodeSource::new(migrated_db("both-v2", 200));
        assert_eq!(
            tie_or_v2_newer.transcript("ses_copied").unwrap()[0].text,
            "from v2"
        );
        let v1_newer = OpencodeSource::new(migrated_db("both-v1", 500));
        assert_eq!(
            v1_newer.transcript("ses_copied").unwrap()[0].text,
            "from v1"
        );
        let listed = v1_newer.list().unwrap();
        let copied = listed.iter().find(|s| s.id == "ses_copied").unwrap();
        assert_eq!(copied.updated_ms, 500);
    }

    #[test]
    fn model_json_parsing() {
        assert_eq!(
            model_name(r#"{"id":"gpt-5.5","providerID":"openai"}"#).as_deref(),
            Some("gpt-5.5")
        );
        assert_eq!(model_name("plain-model").as_deref(), Some("plain-model"));
        assert_eq!(model_name(""), None);
    }

    #[test]
    fn missing_db_is_unavailable() {
        let source = OpencodeSource::new(PathBuf::from("/nonexistent/opencode.db"));
        assert!(!source.available());
        assert!(source.list().is_err());
    }
}
