use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::model::{clean_title, parse_iso_utc_ms, Session, Tool};
use crate::sources::{SessionSource, Turn};

pub struct CopilotSource {
    root: PathBuf,
}

impl CopilotSource {
    pub fn from_env() -> Self {
        let root = std::env::var("COPILOT_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_default();
                Path::new(&home).join(".copilot")
            });
        Self::new(root)
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("session-store.db")
    }

    fn connect(&self) -> Result<Connection, String> {
        let connection = Connection::open_with_flags(
            self.db_path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(3))
            .map_err(|error| error.to_string())?;
        Ok(connection)
    }
}

impl SessionSource for CopilotSource {
    fn tool(&self) -> Tool {
        Tool::Copilot
    }

    fn available(&self) -> bool {
        self.db_path().is_file() || self.root.join("session-state").is_dir()
    }

    fn list(&self) -> Result<Vec<Session>, String> {
        if self.db_path().is_file() {
            return list_from_db(&self.root, &self.connect()?);
        }
        Ok(Vec::new())
    }

    fn transcript(&self, id: &str) -> Result<Vec<Turn>, String> {
        let events = self
            .root
            .join("session-state")
            .join(id)
            .join("events.jsonl");
        if events.is_file() {
            let turns = read_event_turns(&events).map_err(|error| error.to_string())?;
            if !turns.is_empty() {
                return Ok(turns);
            }
        }
        if self.db_path().is_file() {
            return turns_from_db(&self.connect()?, id);
        }
        Err(format!("copilot session {id} not found"))
    }
}

fn list_from_db(root: &Path, connection: &Connection) -> Result<Vec<Session>, String> {
    let mut statement = connection
        .prepare(
            "SELECT id, cwd, repository, branch, summary, created_at, updated_at,
                    (SELECT count(*) FROM turns t WHERE t.session_id = sessions.id)
             FROM sessions
             ORDER BY updated_at DESC",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let repository: Option<String> = row.get(2)?;
            let branch: Option<String> = row.get(3)?;
            let mut extras = Vec::new();
            if let Some(repository) = repository.filter(|value| !value.is_empty()) {
                extras.push(("repo".to_string(), repository));
            }
            if let Some(branch) = branch.filter(|value| !value.is_empty()) {
                extras.push(("branch".to_string(), branch));
            }
            Ok(Session {
                tool: Tool::Copilot,
                id: id.clone(),
                title: row
                    .get::<_, Option<String>>(4)?
                    .map(|value| clean_title(&value))
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "(no title)".to_string()),
                cwd: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                created_ms: row
                    .get::<_, Option<String>>(5)?
                    .as_deref()
                    .and_then(parse_copilot_time_ms)
                    .unwrap_or(0),
                updated_ms: row
                    .get::<_, Option<String>>(6)?
                    .as_deref()
                    .and_then(parse_copilot_time_ms)
                    .unwrap_or(0),
                message_count: row.get::<_, Option<i64>>(7)?.unwrap_or(0) as u32 * 2,
                model: None,
                source_ref: root.join("session-state").join(&id).display().to_string(),
                extras,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

fn turns_from_db(connection: &Connection, id: &str) -> Result<Vec<Turn>, String> {
    let mut statement = connection
        .prepare(
            "SELECT user_message, assistant_response FROM turns WHERE session_id = ?1 ORDER BY turn_index",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut turns = Vec::new();
    for row in rows.flatten() {
        if let Some(text) = row.0.filter(|value| !value.is_empty()) {
            turns.push(Turn {
                role: "user".to_string(),
                text,
            });
        }
        if let Some(text) = row.1.filter(|value| !value.is_empty()) {
            turns.push(Turn {
                role: "assistant".to_string(),
                text,
            });
        }
    }
    Ok(turns)
}

fn read_event_turns(path: &Path) -> Result<Vec<Turn>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let mut turns = Vec::new();
    for line in content.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(turn) = copilot_event_turn(&event) {
            turns.push(turn);
        }
    }
    Ok(turns)
}

fn copilot_event_turn(event: &Value) -> Option<Turn> {
    match event.get("type").and_then(Value::as_str)? {
        "message" => {
            let role = event.get("role")?.as_str()?;
            let text = event
                .get("parts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then(|| Turn {
                role: role.to_string(),
                text,
            })
        }
        "user.message" => copilot_data_content(event).map(|text| Turn {
            role: "user".to_string(),
            text,
        }),
        "assistant.message" => copilot_data_content(event).map(|text| Turn {
            role: "assistant".to_string(),
            text,
        }),
        "tool.execution_start" => {
            let data = event.get("data")?;
            let name = data
                .get("toolName")
                .or_else(|| data.get("mcpToolName"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let args = data
                .get("arguments")
                .map(Value::to_string)
                .unwrap_or_else(|| "{}".to_string());
            Some(Turn {
                role: "tool".to_string(),
                text: format!("call {name} {args}"),
            })
        }
        "tool.execution_complete" => {
            let data = event.get("data")?;
            let status = if data.get("success").and_then(Value::as_bool) == Some(false) {
                "error"
            } else {
                "result"
            };
            let text = data
                .get("result")
                .or_else(|| data.get("error"))
                .map(value_preview)
                .unwrap_or_default();
            Some(Turn {
                role: "tool".to_string(),
                text: format!("{status} {text}"),
            })
        }
        "permission.requested" => Some(Turn {
            role: "system".to_string(),
            text: "permission requested".to_string(),
        }),
        _ => None,
    }
}

fn copilot_data_content(event: &Value) -> Option<String> {
    let data = event.get("data")?;
    let content = data
        .get("transformedContent")
        .or_else(|| data.get("content"))?;
    value_text(content)
}

fn value_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
        return Some(text.to_string());
    }
    let text = value
        .as_array()?
        .iter()
        .filter_map(|item| {
            item.get("text")
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn value_preview(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    value.to_string()
}

fn parse_copilot_time_ms(value: &str) -> Option<i64> {
    parse_iso_utc_ms(value).or_else(|| parse_iso_utc_ms(&format!("{value}Z")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sessiongator-copilot-source-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("session-state/ses_demo")).unwrap();
        let connection = Connection::open(root.join("session-store.db")).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT, branch TEXT, summary TEXT, created_at TEXT, updated_at TEXT);
                CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, turn_index INTEGER, user_message TEXT, assistant_response TEXT, timestamp TEXT);
                INSERT INTO sessions VALUES ('ses_demo', '/tmp/copilot-demo', '/tmp/copilot-demo', 'local', 'main', 'Copilot demo', '2026-07-03T10:00:01.000Z', '2026-07-03T10:00:02.000Z');
                INSERT INTO turns (session_id, turn_index, user_message, assistant_response, timestamp) VALUES ('ses_demo', 0, 'hello copilot', 'hi back', '2026-07-03T10:00:01.000Z');
                "#,
            )
            .unwrap();
        std::fs::write(
            root.join("session-state/ses_demo/events.jsonl"),
            r#"{"type":"message","role":"user","parts":[{"type":"text","text":"event hello"}]}
{"type":"message","role":"assistant","parts":[{"type":"reasoning","text":"hidden"},{"type":"text","text":"event hi"}]}"#,
        )
        .unwrap();
        root
    }

    #[test]
    fn lists_and_reads_copilot_sessions() {
        let root = fixture_root("list");
        let source = CopilotSource::new(root);
        let sessions = source.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].tool, Tool::Copilot);
        assert_eq!(sessions[0].title, "Copilot demo");
        assert_eq!(sessions[0].cwd, "/tmp/copilot-demo");
        let turns = source.transcript("ses_demo").unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "event hello");
        assert_eq!(turns[1].text, "event hi");
    }

    #[test]
    fn reads_native_copilot_user_and_assistant_messages() {
        let root = fixture_root("native");
        std::fs::write(
            root.join("session-state/ses_demo/events.jsonl"),
            r#"{"type":"session.start","timestamp":"2026-07-03T10:00:00.000Z","data":{"sessionId":"ses_demo"}}
{"type":"user.message","timestamp":"2026-07-03T10:00:01.000Z","data":{"content":"native user","transformedContent":"native user transformed"}}
{"type":"assistant.message","timestamp":"2026-07-03T10:00:02.000Z","data":{"phase":"final_answer","content":"native assistant"}}
{"type":"tool.execution_start","timestamp":"2026-07-03T10:00:03.000Z","data":{"toolCallId":"tool_1","toolName":"shell","arguments":{"cmd":"ls"}}}
{"type":"tool.execution_complete","timestamp":"2026-07-03T10:00:04.000Z","data":{"toolCallId":"tool_1","success":true,"result":"ok"}}
{"type":"system.message","timestamp":"2026-07-03T10:00:03.000Z","data":{"role":"system","content":"hidden system"}}"#,
        )
        .unwrap();
        let source = CopilotSource::new(root);
        let turns = source.transcript("ses_demo").unwrap();
        assert_eq!(turns.len(), 4);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "native user transformed");
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].text, "native assistant");
        assert_eq!(turns[2].role, "tool");
        assert!(turns[2].text.contains("call shell"));
        assert_eq!(turns[3].role, "tool");
        assert!(turns[3].text.contains("result ok"));
    }
}
