//! GitHub Copilot CLI: `<root>/session-state/<id>/events.jsonl` is the
//! session event log; `<root>/session-store.db` holds list metadata and turn
//! projections for search.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
};

use gator::AppResult;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::sqlite::{
    open_read_only, open_read_write, schema_fingerprint, table_exists, tables_with_column,
};
use super::{
    append_lines, env_path, home, Fingerprint, Footprint, LineSet, Note, RowSet, Store,
    WriteRequest, Written,
};
use crate::model::{clean_title, Session, Tool};
use crate::native::{
    first_text_part, generated_id, generated_uuid, non_empty_string, parts_from_json, parts_text,
    parts_to_json, title_from_text, NativeMessage, NativePart, NativeRole, NativeSession,
};
use crate::time::{iso_utc, now_ms, parse_flexible_time_ms, parse_iso_utc_ms};

pub fn default_root() -> PathBuf {
    env_path("COPILOT_HOME").unwrap_or_else(|| home().join(".copilot"))
}

pub struct CopilotStore {
    root: PathBuf,
}

impl CopilotStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("session-store.db")
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.root.join("session-state").join(id)
    }

    fn events_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("events.jsonl")
    }

    fn write_into(
        &self,
        tmp: &std::path::Path,
        dir: &std::path::Path,
        request: &WriteRequest,
    ) -> AppResult<Written> {
        let session = request.session;
        fs::create_dir_all(tmp)?;
        fs::write(
            tmp.join("events.jsonl"),
            event_lines(session, request.cli_version).join("\n") + "\n",
        )?;
        fs::write(tmp.join("workspace.yaml"), workspace_yaml(session)?)?;
        create_session_db(&tmp.join("session.db"))?;
        let mut connection = open_read_write(&self.db_path())?;
        create_store_schema(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_store_rows(&transaction, request)?;
        if dir.exists() {
            // Only reachable with --force.
            fs::remove_dir_all(dir)?;
        }
        fs::rename(tmp, dir)?;
        transaction.commit()?;
        Ok(Written {
            reference: dir.display().to_string(),
            backup: None,
        })
    }

    fn list_db(&self) -> AppResult<Vec<Session>> {
        let connection = open_read_only(&self.db_path())?;
        let mut statement = connection.prepare(
            "SELECT id, cwd, repository, branch, summary, created_at, updated_at,
                    (SELECT count(*) FROM turns t WHERE t.session_id = sessions.id)
             FROM sessions",
        )?;
        let rows = statement.query_map([], |row| {
            let id: String = row.get(0)?;
            let repository: Option<String> = row.get(2)?;
            let branch: Option<String> = row.get(3)?;
            let time = |value: Option<String>| {
                value
                    .as_deref()
                    .and_then(parse_flexible_time_ms)
                    .unwrap_or(0)
            };
            Ok(Session {
                tool: Tool::Copilot,
                source_ref: self.session_dir(&id).display().to_string(),
                id,
                title: row
                    .get::<_, Option<String>>(4)?
                    .map(|value| clean_title(&value))
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "(no title)".to_string()),
                cwd: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                created_ms: time(row.get(5)?),
                updated_ms: time(row.get(6)?),
                // One turn is one user message plus one answer.
                message_count: row.get::<_, i64>(7)? as u32 * 2,
                model: None,
                extras: repo_extras(repository, branch),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// List metadata from `workspace.yaml` and the event log of one session.
    fn list_state_dir(&self, dir: &std::path::Path) -> Option<Session> {
        let workspace = fs::read_to_string(dir.join("workspace.yaml")).unwrap_or_default();
        let field = |key: &str| yaml_scalar(&workspace, key);
        let id = field("id").or_else(|| dir.file_name()?.to_str().map(str::to_string))?;
        let log = read_events(&dir.join("events.jsonl")).ok()?;
        let visible = log
            .messages
            .iter()
            .filter(|message| matches!(message.role, NativeRole::User | NativeRole::Assistant))
            .filter(|message| !parts_text(&message.parts).is_empty());
        let message_count = visible.clone().count() as u32;
        let first_user = log
            .messages
            .iter()
            .find(|message| message.role == NativeRole::User)
            .and_then(|message| first_text_part(&message.parts));
        let last_event = log.messages.last().map(|message| message.created_ms);
        let created = field("created_at").and_then(|value| parse_flexible_time_ms(&value));
        let updated = field("updated_at")
            .and_then(|value| parse_flexible_time_ms(&value))
            .into_iter()
            .chain(last_event)
            .max();
        // A `name` that Copilot did not get from the user is often a pasted
        // prompt; the first user message is the better title then.
        let named = field("user_named").as_deref() == Some("true");
        let title = field("name")
            .filter(|_| named)
            .or_else(|| first_user.map(|text| title_from_text(&text)))
            .map(|text| clean_title(&text))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "(no title)".to_string());
        let updated_ms = updated.or(created).unwrap_or(0);
        Some(Session {
            tool: Tool::Copilot,
            id,
            title,
            cwd: field("cwd").or(log.cwd).unwrap_or_default(),
            created_ms: created.unwrap_or(updated_ms),
            updated_ms,
            message_count,
            model: None,
            source_ref: dir.display().to_string(),
            extras: repo_extras(field("repository"), field("branch")),
        })
    }
}

fn repo_extras(repository: Option<String>, branch: Option<String>) -> Vec<(String, String)> {
    [("repo", repository), ("branch", branch)]
        .into_iter()
        .filter_map(|(key, value)| {
            value
                .filter(|value| !value.is_empty())
                .map(|value| (key.to_string(), value))
        })
        .collect()
}

/// A top-level `key: value` scalar from `workspace.yaml`. Copilot writes a
/// flat mapping; block scalars (`|-`) such as a long `name` read as absent.
/// Limit: no YAML escapes beyond JSON-style double quotes; switch to a YAML
/// parser if Copilot starts nesting or escaping these keys.
fn yaml_scalar(yaml: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let raw = yaml
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))?
        .trim();
    if raw.is_empty() || raw.starts_with('|') || raw.starts_with('>') {
        return None;
    }
    if raw.starts_with('"') {
        return serde_json::from_str(raw).ok();
    }
    Some(raw.trim_matches('\'').to_string())
}

impl Store for CopilotStore {
    fn tool(&self) -> Tool {
        Tool::Copilot
    }

    fn available(&self) -> bool {
        self.db_path().is_file() || self.root.join("session-state").is_dir()
    }

    /// Sessions with an event log, merged with `session-store.db` rows.
    /// Copilot does not record every session in the database, and a folder
    /// without `events.jsonl` holds nothing to resume.
    fn list(&self) -> AppResult<Vec<Session>> {
        let mut by_id: HashMap<String, Session> = HashMap::new();
        if self.db_path().is_file() {
            for session in self.list_db()? {
                by_id.insert(session.id.clone(), session);
            }
        }
        let dirs = fs::read_dir(self.root.join("session-state"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|dir| dir.join("events.jsonl").is_file());
        for dir in dirs {
            let Some(logged) = self.list_state_dir(&dir) else {
                continue;
            };
            match by_id.get_mut(&logged.id) {
                Some(row) => {
                    row.updated_ms = row.updated_ms.max(logged.updated_ms);
                    row.message_count = logged.message_count;
                    if row.cwd.is_empty() {
                        row.cwd = logged.cwd;
                    }
                }
                None => {
                    by_id.insert(logged.id.clone(), logged);
                }
            }
        }
        Ok(by_id.into_values().collect())
    }

    fn read(&self, id: &str) -> AppResult<NativeSession> {
        let mut title = None;
        let mut cwd = None;
        let mut created_ms = None;
        let mut updated_ms = None;
        let mut messages = Vec::new();
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "source_ref".to_string(),
            json!(self.session_dir(id).display().to_string()),
        );
        let mut found = false;

        if self.db_path().is_file() {
            let connection = open_read_only(&self.db_path())?;
            if let Some((session_cwd, summary, created, updated, repository, branch)) = connection
                .query_row(
                    "SELECT cwd, summary, created_at, updated_at, repository, branch FROM sessions WHERE id = ?1",
                    [id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .optional()?
            {
                found = true;
                cwd = session_cwd;
                title = summary;
                created_ms = created.and_then(|value| parse_flexible_time_ms(&value));
                updated_ms = updated.and_then(|value| parse_flexible_time_ms(&value));
                if let Some(repository) = repository {
                    metadata.insert("repository".to_string(), json!(repository));
                }
                if let Some(branch) = branch {
                    metadata.insert("branch".to_string(), json!(branch));
                }
                messages = read_turns(&connection, id)?;
            }
        }

        // The event log is the full session; DB turns are a text projection.
        let events = self.events_path(id);
        if events.is_file() {
            found = true;
            let log = read_events(&events)?;
            if let Some(version) = log.copilot_version {
                metadata.insert("copilot_version".to_string(), json!(version));
            }
            cwd = cwd.or(log.cwd);
            if !log.messages.is_empty() {
                messages = log.messages;
            }
        }
        if !found {
            return Err(format!("copilot session {id} not found").into());
        }

        let first_user = messages
            .iter()
            .find(|message| message.role == NativeRole::User)
            .and_then(|message| first_text_part(&message.parts));
        if let (Some(first), Some(last)) = (messages.first(), messages.last()) {
            created_ms.get_or_insert(first.created_ms);
            updated_ms.get_or_insert(last.updated_ms.unwrap_or(last.created_ms));
        }
        let updated = updated_ms.or(created_ms).unwrap_or_else(now_ms);
        Ok(NativeSession {
            tool: Tool::Copilot,
            id: id.to_string(),
            title: title
                .filter(|title| !title.is_empty())
                .or_else(|| first_user.map(|text| title_from_text(&text)))
                .unwrap_or_else(|| "(imported session)".to_string()),
            cwd: cwd.unwrap_or_default(),
            created_ms: created_ms.unwrap_or(updated),
            updated_ms: updated,
            model: None,
            messages,
            metadata,
        })
    }

    fn fingerprint(&self) -> AppResult<Fingerprint> {
        if !self.db_path().is_file() {
            return Ok(Fingerprint {
                store_version: None,
                schema: None,
            });
        }
        let connection = open_read_only(&self.db_path())?;
        let store_version = if table_exists(&connection, "schema_version")? {
            connection
                .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?
                .map(|version| version.to_string())
        } else {
            None
        };
        Ok(Fingerprint {
            store_version,
            schema: Some(schema_fingerprint(&connection)?),
        })
    }

    /// Files go to `session-state/<id>.tmp/` first, and the folder is renamed
    /// into place inside the open database transaction, so a failure leaves
    /// neither a half-written folder nor rows without a folder.
    fn write(&self, request: &WriteRequest) -> AppResult<Written> {
        let session = request.session;
        let dir = self.session_dir(&session.id);
        if dir.exists() && !request.force {
            return Err(format!("target Copilot session already exists: {}", dir.display()).into());
        }
        let tmp = dir.with_extension("tmp");
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        let result = self.write_into(&tmp, &dir, request);
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        result
    }

    fn footprint(&self, id: &str) -> AppResult<Footprint> {
        let mut footprint = Footprint::default();
        let dir = self.session_dir(id);
        if dir.exists() {
            footprint.paths.push(dir);
        }
        if self.db_path().is_file() {
            let connection = open_read_only(&self.db_path())?;
            let row_set = |table: &str, column: &str| RowSet {
                db: self.db_path(),
                table: table.to_string(),
                column: column.to_string(),
                value: id.to_string(),
            };
            for table in tables_with_column(&connection, "session_id")? {
                footprint.rows.push(row_set(&table, "session_id"));
            }
            if table_exists(&connection, "sessions")? {
                footprint.rows.push(row_set("sessions", "id"));
            }
        }
        if footprint.paths.is_empty() {
            return Err(format!("copilot session {id} not found").into());
        }
        Ok(footprint)
    }

    fn append_note(&self, id: &str, note: &Note) -> AppResult<Footprint> {
        let path = self.events_path(id);
        if !path.is_file() {
            return Err(format!(
                "copilot session {id} has no event log at {}",
                path.display()
            )
            .into());
        }
        let parent = last_event_id(&path)?;
        let message = NativeMessage::text(NativeRole::Assistant, now_ms(), note.text.to_string());
        let line = message_event(&message, "sessiongator", 0, parent.as_deref())
            .expect("an assistant text message has an event")
            .0
            .to_string();
        append_lines(&path, std::slice::from_ref(&line))?;
        Ok(Footprint {
            lines: vec![LineSet {
                file: path,
                lines: vec![line],
            }],
            ..Footprint::default()
        })
    }
}

fn read_turns(connection: &Connection, id: &str) -> AppResult<Vec<NativeMessage>> {
    if !table_exists(connection, "turns")? {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT turn_index, user_message, assistant_response, timestamp FROM turns WHERE session_id = ?1 ORDER BY turn_index",
    )?;
    let rows = statement.query_map([id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (index, user, assistant, timestamp) = row?;
        let created = timestamp
            .and_then(|value| parse_flexible_time_ms(&value))
            .unwrap_or_else(now_ms);
        // Keep user before assistant when turns share a timestamp.
        if let Some(user) = user.filter(|value| !value.is_empty()) {
            messages.push(NativeMessage::text(
                NativeRole::User,
                created + index * 2,
                user,
            ));
        }
        if let Some(assistant) = assistant.filter(|value| !value.is_empty()) {
            messages.push(NativeMessage::text(
                NativeRole::Assistant,
                created + index * 2 + 1,
                assistant,
            ));
        }
    }
    Ok(messages)
}

struct EventLog {
    copilot_version: Option<String>,
    cwd: Option<String>,
    messages: Vec<NativeMessage>,
}

fn read_events(path: &std::path::Path) -> AppResult<EventLog> {
    let mut log = EventLog {
        copilot_version: None,
        cwd: None,
        messages: Vec::new(),
    };
    for line in BufReader::new(fs::File::open(path)?).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("session.start") {
            log.copilot_version = non_empty_string(event.pointer("/data/copilotVersion"));
            log.cwd = non_empty_string(event.pointer("/data/context/cwd"));
            continue;
        }
        if let Some(message) = event_message(&event) {
            log.messages.push(message);
        }
    }
    Ok(log)
}

fn last_event_id(path: &std::path::Path) -> AppResult<Option<String>> {
    let content = fs::read_to_string(path)?;
    Ok(content.lines().rev().find_map(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|event| non_empty_string(event.get("id")))
    }))
}

fn event_message(event: &Value) -> Option<NativeMessage> {
    let timestamp = non_empty_string(event.get("timestamp"))
        .and_then(|value| parse_iso_utc_ms(&value))
        .unwrap_or_else(now_ms);
    let data = event.get("data").unwrap_or(event);
    let message = |role, part| NativeMessage {
        role,
        created_ms: timestamp,
        updated_ms: None,
        parts: vec![part],
        metadata: BTreeMap::new(),
    };
    match event.get("type").and_then(Value::as_str)? {
        kind @ ("user.message" | "assistant.message") => {
            let role = if kind == "user.message" {
                NativeRole::User
            } else {
                NativeRole::Assistant
            };
            let text = data
                .get("transformedContent")
                .or_else(|| data.get("content"))
                .and_then(content_text)?;
            let parts = data
                .get("sessiongatorParts")
                .and_then(parts_from_json)
                .unwrap_or_else(|| vec![NativePart::Text(text)]);
            Some(NativeMessage {
                role,
                created_ms: timestamp,
                updated_ms: None,
                parts,
                metadata: BTreeMap::new(),
            })
        }
        "tool.execution_start" => Some(message(
            NativeRole::Assistant,
            NativePart::ToolCall {
                id: non_empty_string(data.get("toolCallId"))
                    .unwrap_or_else(|| generated_id("tool")),
                name: non_empty_string(data.get("toolName"))
                    .or_else(|| non_empty_string(data.get("mcpToolName")))
                    .unwrap_or_else(|| "unknown".to_string()),
                input: data.get("arguments").cloned().unwrap_or(Value::Null),
            },
        )),
        "tool.execution_complete" => Some(message(
            NativeRole::User,
            NativePart::ToolResult {
                id: non_empty_string(data.get("toolCallId"))
                    .unwrap_or_else(|| generated_id("tool")),
                content: data
                    .get("result")
                    .or_else(|| data.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null),
                is_error: data.get("success").and_then(Value::as_bool) == Some(false),
            },
        )),
        "permission.requested" => {
            let request = data
                .get("permissionRequest")
                .or_else(|| data.get("promptRequest"))
                .map(Value::to_string)
                .unwrap_or_else(|| "{}".to_string());
            Some(message(
                NativeRole::System,
                NativePart::Text(format!("permission requested {request}")),
            ))
        }
        "session.shutdown" => data.get("conversationTokens").map(|tokens| {
            let kind =
                non_empty_string(data.get("shutdownType")).unwrap_or_else(|| "unknown".to_string());
            message(
                NativeRole::Compaction,
                NativePart::Text(format!(
                    "session shutdown: {kind}; conversationTokens={tokens}"
                )),
            )
        }),
        _ => None,
    }
}

/// Message content: a string, or an array of `{text}` / `{content}` items.
fn content_text(value: &Value) -> Option<String> {
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

/// One `user.message` or `assistant.message` event and its id. Other roles
/// have no Copilot event form.
fn message_event(
    message: &NativeMessage,
    model: &str,
    turn: usize,
    parent: Option<&str>,
) -> Option<(Value, String)> {
    let content = parts_text(&message.parts);
    let interaction_id = generated_uuid();
    let (kind, data) = match message.role {
        NativeRole::User => (
            "user.message",
            json!({
                "content": content,
                "transformedContent": content,
                "attachments": [],
                "supportedNativeDocumentMimeTypes": [],
                "agentMode": "interactive",
                "interactionId": interaction_id,
                "sessiongatorParts": parts_to_json(&message.parts),
            }),
        ),
        NativeRole::Assistant => (
            "assistant.message",
            json!({
                "messageId": generated_uuid(),
                "model": model,
                "content": content,
                "toolRequests": [],
                "interactionId": interaction_id,
                "turnId": turn.to_string(),
                "sessiongatorParts": parts_to_json(&message.parts),
            }),
        ),
        _ => return None,
    };
    let id = generated_uuid();
    Some((
        json!({
            "type": kind,
            "data": data,
            "id": id,
            "timestamp": iso_utc(message.created_ms),
            "parentId": parent,
        }),
        id,
    ))
}

fn event_lines(session: &NativeSession, cli_version: &str) -> Vec<String> {
    let start_id = generated_uuid();
    let model = session
        .model
        .as_ref()
        .map(|model| model.id.as_str())
        .unwrap_or("imported");
    let mut lines = vec![json!({
        "type": "session.start",
        "data": {
            "sessionId": session.id,
            "version": 1,
            "producer": "sessiongator",
            "copilotVersion": cli_version,
            "startTime": iso_utc(session.created_ms),
            "selectedModel": session.model.as_ref().map(|model| model.id.as_str()).unwrap_or("auto"),
            "context": {
                "cwd": session.cwd,
                "gitRoot": session.cwd,
                "repository": session.cwd,
                "hostType": "github"
            },
            "alreadyInUse": false
        },
        "id": start_id,
        "timestamp": iso_utc(session.created_ms),
        "parentId": null
    })
    .to_string()];
    let mut parent = start_id;
    for (turn, message) in session.messages.iter().enumerate() {
        if let Some((event, id)) = message_event(message, model, turn, Some(&parent)) {
            lines.push(event.to_string());
            parent = id;
        }
    }
    lines
}

fn workspace_yaml(session: &NativeSession) -> AppResult<String> {
    let quoted = |value: &str| serde_json::to_string(value);
    Ok([
        format!("id: {}", quoted(&session.id)?),
        format!("cwd: {}", quoted(&session.cwd)?),
        format!("git_root: {}", quoted(&session.cwd)?),
        format!("repository: {}", quoted(&session.cwd)?),
        "host_type: github".to_string(),
        "branch: \"\"".to_string(),
        "client_name: sessiongator".to_string(),
        format!("name: {}", quoted(&session.title)?),
        "user_named: true".to_string(),
        "summary_count: 0".to_string(),
        format!("created_at: {}", iso_utc(session.created_ms)),
        format!("updated_at: {}", iso_utc(session.updated_ms)),
    ]
    .join("\n")
        + "\n")
}

fn create_session_db(path: &std::path::Path) -> AppResult<()> {
    Connection::open(path)?.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS todos (id TEXT PRIMARY KEY, title TEXT NOT NULL, description TEXT, status TEXT DEFAULT 'pending' CHECK(status IN ('pending', 'in_progress', 'done', 'blocked')), created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));
        CREATE TABLE IF NOT EXISTS todo_deps (todo_id TEXT NOT NULL, depends_on TEXT NOT NULL, PRIMARY KEY (todo_id, depends_on), FOREIGN KEY (todo_id) REFERENCES todos(id), FOREIGN KEY (depends_on) REFERENCES todos(id));
        CREATE TABLE IF NOT EXISTS inbox_entries (id TEXT PRIMARY KEY, recipient_session_id TEXT NOT NULL, sender_id TEXT NOT NULL, sender_name TEXT NOT NULL, sender_type TEXT NOT NULL, interaction_id TEXT NOT NULL, sequence INTEGER NOT NULL DEFAULT 0, summary TEXT NOT NULL, content TEXT NOT NULL, unread INTEGER NOT NULL DEFAULT 1, sent_at INTEGER NOT NULL, read_at INTEGER, notified_at INTEGER);
        "#,
    )?;
    Ok(())
}

/// The session-store.db tables sessiongator writes, in the version 6 shape.
fn create_store_schema(connection: &Connection) -> AppResult<()> {
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT, branch TEXT, summary TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));
        CREATE TABLE IF NOT EXISTS turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT, timestamp TEXT DEFAULT (datetime('now')), UNIQUE(session_id, turn_index));
        "#,
    )?;
    let count: i64 =
        connection.query_row("SELECT count(*) FROM schema_version", [], |row| row.get(0))?;
    if count == 0 {
        connection.execute("INSERT INTO schema_version (version) VALUES (6)", [])?;
    }
    Ok(())
}

fn write_store_rows(connection: &Connection, request: &WriteRequest) -> AppResult<()> {
    let session = request.session;
    let exists = connection
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ?1",
            [session.id.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        if !request.force {
            return Err(format!("target Copilot session already exists: {}", session.id).into());
        }
        connection.execute(
            "DELETE FROM turns WHERE session_id = ?1",
            [session.id.as_str()],
        )?;
        connection.execute("DELETE FROM sessions WHERE id = ?1", [session.id.as_str()])?;
    }
    connection.execute(
        "INSERT INTO sessions (id, cwd, repository, host_type, branch, summary, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            session.id,
            session.cwd,
            session.cwd,
            "github",
            session.metadata.get("gitBranch").and_then(Value::as_str),
            session.title,
            iso_utc(session.created_ms),
            iso_utc(session.updated_ms),
        ],
    )?;
    // Turns pair each user message with the next assistant message.
    let mut turn = 0i64;
    let mut pending_user: Option<&NativeMessage> = None;
    let visible = session
        .messages
        .iter()
        .filter(|message| !parts_text(&message.parts).is_empty());
    for message in visible {
        match message.role {
            NativeRole::User => {
                if let Some(user) = pending_user.replace(message) {
                    insert_turn(connection, &session.id, turn, Some(user), None)?;
                    turn += 1;
                }
            }
            NativeRole::Assistant => {
                insert_turn(
                    connection,
                    &session.id,
                    turn,
                    pending_user.take(),
                    Some(message),
                )?;
                turn += 1;
            }
            _ => {}
        }
    }
    if let Some(user) = pending_user {
        insert_turn(connection, &session.id, turn, Some(user), None)?;
    }
    Ok(())
}

fn insert_turn(
    connection: &Connection,
    session_id: &str,
    turn: i64,
    user: Option<&NativeMessage>,
    assistant: Option<&NativeMessage>,
) -> rusqlite::Result<()> {
    let created = user
        .or(assistant)
        .map_or_else(now_ms, |message| message.created_ms);
    connection.execute(
        "INSERT INTO turns (session_id, turn_index, user_message, assistant_response, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            session_id,
            turn,
            user.map(|message| parts_text(&message.parts)),
            assistant.map(|message| parts_text(&message.parts)),
            iso_utc(created)
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::turns;
    use crate::stores::tests::{sample_session, temp_dir};

    const ID: &str = "44444444-5555-4666-8777-888888888888";

    fn native_store(name: &str) -> CopilotStore {
        let root = temp_dir(&format!("copilot-{name}"));
        let dir = root.join("session-state/ses_demo");
        fs::create_dir_all(&dir).unwrap();
        let connection = Connection::open(root.join("session-store.db")).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT, branch TEXT, summary TEXT, created_at TEXT, updated_at TEXT);
                CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, turn_index INTEGER, user_message TEXT, assistant_response TEXT, timestamp TEXT);
                CREATE TABLE checkpoints (session_id TEXT, title TEXT);
                INSERT INTO sessions VALUES ('ses_demo', '/tmp/copilot-demo', 'org/repo', 'github', 'main', 'Copilot demo', '2026-07-03T10:00:01.000Z', '2026-07-03 10:00:02');
                INSERT INTO turns (session_id, turn_index, user_message, assistant_response, timestamp) VALUES ('ses_demo', 0, 'db hello', 'db hi', '2026-07-03T10:00:01.000Z');
                "#,
            )
            .unwrap();
        fs::write(
            dir.join("events.jsonl"),
            r#"{"type":"session.start","id":"e0","timestamp":"2026-07-03T10:00:00.000Z","data":{"sessionId":"ses_demo","copilotVersion":"1.0.96","context":{"cwd":"/tmp/copilot-demo"}}}
{"type":"user.message","id":"e1","timestamp":"2026-07-03T10:00:01.000Z","data":{"content":"native user","transformedContent":"native user transformed"}}
{"type":"assistant.message","id":"e2","timestamp":"2026-07-03T10:00:02.000Z","data":{"phase":"final_answer","content":"native assistant"}}
{"type":"tool.execution_start","id":"e3","timestamp":"2026-07-03T10:00:03.000Z","data":{"toolCallId":"tool_1","toolName":"shell","arguments":{"cmd":"ls"}}}
{"type":"tool.execution_complete","id":"e4","timestamp":"2026-07-03T10:00:04.000Z","data":{"toolCallId":"tool_1","success":true,"result":"ok"}}
{"type":"system.message","id":"e5","timestamp":"2026-07-03T10:00:03.000Z","data":{"role":"system","content":"hidden system"}}"#,
        )
        .unwrap();
        CopilotStore::new(root)
    }

    #[test]
    fn lists_db_rows_and_sessions_that_only_have_an_event_log() {
        let store = native_store("list");
        let only_log = store.session_dir("ses_log");
        fs::create_dir_all(&only_log).unwrap();
        fs::write(
            only_log.join("workspace.yaml"),
            "id: ses_log\ncwd: /tmp/log-only\nname: |-\n  <pasted prompt>\nuser_named: false\nbranch: dev\ncreated_at: 2026-10-13T19:16:27.777Z\nupdated_at: 2026-10-13T19:16:32.238Z\n",
        )
        .unwrap();
        fs::write(
            only_log.join("events.jsonl"),
            r#"{"type":"user.message","id":"e1","timestamp":"2026-10-13T19:17:00.000Z","data":{"content":"fix the build"}}"#,
        )
        .unwrap();
        // A folder without an event log has nothing to resume.
        fs::create_dir_all(store.session_dir("ses_empty")).unwrap();

        let mut sessions = store.list().unwrap();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["ses_demo", "ses_log"]);
        let demo = &sessions[0];
        assert_eq!(demo.title, "Copilot demo");
        assert_eq!(demo.message_count, 2);
        assert!(demo.updated_ms > demo.created_ms);
        let log = &sessions[1];
        assert_eq!(log.title, "fix the build");
        assert_eq!(log.cwd, "/tmp/log-only");
        assert_eq!(log.message_count, 1);
        assert_eq!(log.extras, [("branch".to_string(), "dev".to_string())]);
        assert_eq!(
            log.updated_ms,
            parse_iso_utc_ms("2026-10-13T19:17:00.000Z").unwrap()
        );
    }

    #[test]
    fn reads_workspace_scalars() {
        let yaml = "id: \"a b\"\nname: |-\n  long\ncwd: /tmp/x\n";
        assert_eq!(yaml_scalar(yaml, "id").as_deref(), Some("a b"));
        assert_eq!(yaml_scalar(yaml, "name"), None);
        assert_eq!(yaml_scalar(yaml, "cwd").as_deref(), Some("/tmp/x"));
        assert_eq!(yaml_scalar(yaml, "missing"), None);
    }

    #[test]
    fn reads_the_event_log_before_db_turns() {
        let session = native_store("read").read("ses_demo").unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(
            parts_text(&session.messages[0].parts),
            "native user transformed"
        );
        assert!(matches!(
            session.messages[2].parts[0],
            NativePart::ToolCall { .. }
        ));
        assert!(matches!(
            session.messages[3].parts[0],
            NativePart::ToolResult { .. }
        ));
        assert_eq!(session.metadata["copilot_version"], json!("1.0.96"));
        let texts: Vec<String> = turns(&session).into_iter().map(|turn| turn.text).collect();
        assert_eq!(texts, ["native user transformed", "native assistant"]);
    }

    #[test]
    fn writes_events_and_store_rows_that_read_back() {
        let root = temp_dir("copilot-write");
        let store = CopilotStore::new(root.clone());
        let mut session = sample_session(Tool::Copilot, ID);
        session.messages.push(NativeMessage::text(
            NativeRole::Assistant,
            session.created_ms + 1,
            "hi".to_string(),
        ));
        store
            .write(&WriteRequest {
                session: &session,
                cli_version: "1.0.97",
                provenance: &json!({}),
                force: false,
                backup: false,
            })
            .unwrap();
        let events = fs::read_to_string(store.events_path(ID)).unwrap();
        assert!(events.lines().next().unwrap().contains("session.start"));
        let back = store.read(ID).unwrap();
        assert_eq!(back.title, session.title);
        assert_eq!(back.cwd, session.cwd);
        assert_eq!(turns(&back), turns(&session));
        let listed = store.list().unwrap();
        assert_eq!(listed[0].message_count, 2);
    }

    #[test]
    fn note_chains_to_the_last_event() {
        let store = native_store("note");
        let footprint = store
            .append_note(
                "ses_demo",
                &Note {
                    text: "moved",
                    cli_version: "1.0.97",
                },
            )
            .unwrap();
        let line: Value = serde_json::from_str(&footprint.lines[0].lines[0]).unwrap();
        assert_eq!(line["parentId"], "e5");
        assert_eq!(line["type"], "assistant.message");
        assert_eq!(
            store.transcript("ses_demo").unwrap().last().unwrap().text,
            "moved"
        );
    }

    #[test]
    fn footprint_covers_the_state_dir_and_every_session_row() {
        let footprint = native_store("footprint").footprint("ses_demo").unwrap();
        assert_eq!(footprint.paths.len(), 1);
        let tables: Vec<&str> = footprint
            .rows
            .iter()
            .map(|rows| rows.table.as_str())
            .collect();
        assert_eq!(tables, ["checkpoints", "turns", "sessions"]);
    }
}
