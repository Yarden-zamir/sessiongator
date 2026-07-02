use std::{
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

impl SessionSource for OpencodeSource {
    fn tool(&self) -> Tool {
        Tool::Opencode
    }

    fn available(&self) -> bool {
        self.db_path.is_file()
    }

    fn list(&self) -> Result<Vec<Session>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT s.id, s.parent_id, s.directory, s.title, s.agent, s.model,
                        s.time_created, s.time_updated,
                        (SELECT count(*) FROM message m WHERE m.session_id = s.id)
                 FROM session s
                 ORDER BY s.time_updated DESC",
            )
            .map_err(|error| error.to_string())?;
        let db_path = self.db_path.display().to_string();
        let rows = statement
            .query_map([], |row| {
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
                    source_ref: db_path.clone(),
                    extras,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    fn transcript(&self, id: &str) -> Result<Vec<Turn>, String> {
        let connection = self.connect()?;
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
            let Some(text) = part
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            else {
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
            turns.push(Turn {
                role,
                text: text.to_string(),
            });
        }
        Ok(turns)
    }
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

    fn fixture_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sessiongator-opencode-{name}-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
                    directory TEXT, title TEXT, agent TEXT, model TEXT,
                    time_created INTEGER, time_updated INTEGER);
                CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                    time_created INTEGER, time_updated INTEGER, data TEXT);
                CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                    time_created INTEGER, time_updated INTEGER, data TEXT);
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
                "#,
            )
            .unwrap();
        path
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
