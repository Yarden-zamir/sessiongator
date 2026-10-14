//! opencode: one SQLite database. OpenCode 2 writes `session_v2` and
//! `session_message`. OpenCode 1.x wrote `session`, `message`, and `part`;
//! the first OpenCode 2 start copies every 1.x session into the 2 tables once
//! and keeps the 1.x tables, so one id can exist in both. sessiongator reads
//! both generations and writes OpenCode 2 only.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use gator::AppResult;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::sqlite::{
    open_read_only, open_read_write, schema_fingerprint, table_exists, tables_with_column,
};
use super::{env_path, home, Fingerprint, Footprint, Note, RowSet, Store, WriteRequest, Written};
use crate::model::{clean_title, Session, Tool};
use crate::native::{
    content_to_string, generated_id, model_json, non_empty_string, parts_from_json, parts_text,
    parts_to_json, ModelRef, NativeMessage, NativePart, NativeRole, NativeSession,
};
use crate::time::now_ms;

pub fn default_db() -> PathBuf {
    env_path("OPENCODE_DB").unwrap_or_else(|| {
        env_path("XDG_DATA_HOME")
            .unwrap_or_else(|| home().join(".local").join("share"))
            .join("opencode")
            .join("opencode.db")
    })
}

/// The table generation that holds a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Generation {
    V1,
    V2,
}

impl Generation {
    fn session_table(self) -> &'static str {
        match self {
            Self::V1 => "session",
            Self::V2 => "session_v2",
        }
    }

    /// One row per session; `?1` NULL lists all sessions, else only that id.
    /// Columns: id, parent_id, directory, title, agent, model, time_created,
    /// updated, message count. OpenCode 2 does not bump
    /// `session_v2.time_updated` for each message, so "updated" also takes
    /// the newest `session_message` row.
    fn list_sql(self) -> &'static str {
        match self {
            Self::V1 => {
                "SELECT s.id, s.parent_id, s.directory, s.title, s.agent, s.model,
                        s.time_created, s.time_updated,
                        (SELECT count(*) FROM message m WHERE m.session_id = s.id)
                 FROM session s
                 WHERE ?1 IS NULL OR s.id = ?1"
            }
            Self::V2 => {
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

pub struct OpencodeStore {
    db: PathBuf,
}

impl OpencodeStore {
    pub fn new(db: PathBuf) -> Self {
        Self { db }
    }

    /// Write access to an OpenCode 2 database. A database without
    /// `session_v2` is OpenCode 1.x or not initialized; creating the tables
    /// here would make OpenCode 2 rerun its 1.x migration, which first
    /// deletes every row in `event`.
    fn open_v2(&self) -> AppResult<Connection> {
        if !self.db.is_file() {
            return Err(format!(
                "no opencode database at {}; start OpenCode 2 once to create it",
                self.db.display()
            )
            .into());
        }
        let connection = open_read_write(&self.db)?;
        if !table_exists(&connection, "session_v2")? {
            return Err(format!(
                "{} is not an OpenCode 2 database; start OpenCode 2 once to migrate it",
                self.db.display()
            )
            .into());
        }
        Ok(connection)
    }
}

impl Store for OpencodeStore {
    fn tool(&self) -> Tool {
        Tool::Opencode
    }

    fn available(&self) -> bool {
        self.db.is_file()
    }

    fn list(&self) -> AppResult<Vec<Session>> {
        let connection = open_read_only(&self.db)?;
        let source_ref = self.db.display().to_string();
        // V2 runs second so a tie keeps the V2 copy (see `newest_generation`).
        let mut by_id: HashMap<String, Session> = HashMap::new();
        for generation in [Generation::V1, Generation::V2] {
            if !table_exists(&connection, generation.session_table())? {
                continue;
            }
            for session in list_generation(&connection, generation, None, &source_ref)? {
                if by_id
                    .get(&session.id)
                    .is_none_or(|kept| session.updated_ms >= kept.updated_ms)
                {
                    by_id.insert(session.id.clone(), session);
                }
            }
        }
        Ok(by_id.into_values().collect())
    }

    fn read(&self, id: &str) -> AppResult<NativeSession> {
        let connection = open_read_only(&self.db)?;
        let generation = newest_generation(&connection, id)?
            .ok_or_else(|| format!("opencode session {id} not found"))?;
        let table = generation.session_table();
        let (title, cwd, created_ms, updated_ms, model, version): (
            Option<String>,
            String,
            i64,
            i64,
            Option<String>,
            Option<String>,
        ) = connection.query_row(
            &format!("SELECT title, directory, time_created, time_updated, model, version FROM {table} WHERE id = ?1"),
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )?;
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "source_ref".to_string(),
            json!(self.db.display().to_string()),
        );
        if let Some(version) = version {
            metadata.insert("opencode_session_version".to_string(), json!(version));
        }
        let messages = match generation {
            Generation::V2 => read_v2_messages(&connection, id)?,
            Generation::V1 => read_v1_messages(&connection, id)?,
        };
        Ok(NativeSession {
            tool: Tool::Opencode,
            id: id.to_string(),
            title: title.unwrap_or_default(),
            cwd,
            created_ms,
            updated_ms,
            model: model.as_deref().and_then(parse_model),
            messages,
            metadata,
        })
    }

    fn fingerprint(&self) -> AppResult<Fingerprint> {
        if !self.db.is_file() {
            return Ok(Fingerprint {
                store_version: None,
                schema: None,
            });
        }
        let connection = open_read_only(&self.db)?;
        let mut newest: Option<(i64, String)> = None;
        for generation in [Generation::V1, Generation::V2] {
            let table = generation.session_table();
            if !table_exists(&connection, table)? {
                continue;
            }
            let row = connection
                .query_row(
                    &format!("SELECT time_updated, version FROM {table} WHERE version IS NOT NULL AND version != '' ORDER BY time_updated DESC LIMIT 1"),
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some(row) = row {
                if newest.as_ref().is_none_or(|kept| row.0 >= kept.0) {
                    newest = Some(row);
                }
            }
        }
        Ok(Fingerprint {
            store_version: newest.map(|(_, version)| version),
            schema: Some(schema_fingerprint(&connection)?),
        })
    }

    fn write(&self, request: &WriteRequest) -> AppResult<Written> {
        let mut connection = self.open_v2()?;
        let backup = if request.backup {
            let backup = self
                .db
                .with_extension(format!("db.sessiongator-{}.bak", now_ms()));
            // VACUUM INTO copies committed WAL content too; a file copy does not.
            connection.execute("VACUUM INTO ?1", [backup.display().to_string()])?;
            Some(backup)
        } else {
            None
        };
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_session(&transaction, request)?;
        transaction.commit()?;
        Ok(Written {
            reference: self.db.display().to_string(),
            backup,
        })
    }

    fn footprint(&self, id: &str) -> AppResult<Footprint> {
        let connection = open_read_only(&self.db)?;
        if newest_generation(&connection, id)?.is_none() {
            return Err(format!("opencode session {id} not found").into());
        }
        let row_set = |table: &str, column: &str| RowSet {
            db: self.db.clone(),
            table: table.to_string(),
            column: column.to_string(),
            value: id.to_string(),
        };
        let mut rows: Vec<RowSet> = tables_with_column(&connection, "session_id")?
            .iter()
            .map(|table| row_set(table, "session_id"))
            .collect();
        for table in tables_with_column(&connection, "aggregate_id")? {
            rows.push(row_set(&table, "aggregate_id"));
        }
        for generation in [Generation::V2, Generation::V1] {
            if table_exists(&connection, generation.session_table())? {
                rows.push(row_set(generation.session_table(), "id"));
            }
        }
        Ok(Footprint {
            rows,
            ..Footprint::default()
        })
    }

    fn append_note(&self, id: &str, note: &Note) -> AppResult<Footprint> {
        let mut connection = self.open_v2()?;
        if newest_generation(&connection, id)? != Some(Generation::V2) {
            return Err(format!(
                "opencode session {id} is only in the OpenCode 1.x tables; open it in OpenCode 2 once"
            )
            .into());
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq: i64 = transaction.query_row(
            "SELECT coalesce(max(seq), 0) + 1 FROM session_message WHERE session_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        let model = transaction
            .query_row("SELECT model FROM session_v2 WHERE id = ?1", [id], |row| {
                row.get::<_, Option<String>>(0)
            })?
            .as_deref()
            .and_then(parse_model);
        let message = NativeMessage::text(NativeRole::Assistant, now_ms(), note.text.to_string());
        let message_id = insert_message(
            &transaction,
            id,
            model.as_ref(),
            &message,
            seq,
            &HashMap::new(),
        )?;
        bump_sequence(&transaction, id, seq)?;
        transaction.commit()?;
        Ok(Footprint {
            rows: vec![RowSet {
                db: self.db.clone(),
                table: "session_message".to_string(),
                column: "id".to_string(),
                value: message_id,
            }],
            ..Footprint::default()
        })
    }
}

fn list_generation(
    connection: &Connection,
    generation: Generation,
    id: Option<&str>,
    source_ref: &str,
) -> rusqlite::Result<Vec<Session>> {
    let mut statement = connection.prepare(generation.list_sql())?;
    let rows = statement.query_map([id], |row| {
        let parent_id: Option<String> = row.get(1)?;
        let agent: Option<String> = row.get(4)?;
        let model: Option<String> = row.get(5)?;
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
                .map(|value| clean_title(&value))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "(no title)".to_string()),
            cwd: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            created_ms: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            updated_ms: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            message_count: row.get::<_, Option<i64>>(8)?.unwrap_or(0) as u32,
            model: model.as_deref().and_then(parse_model).map(|model| model.id),
            source_ref: source_ref.to_string(),
            extras,
        })
    })?;
    rows.collect()
}

/// The generation with the newer copy of a session; a tie reads as V2, the
/// generation every current OpenCode writes.
fn newest_generation(connection: &Connection, id: &str) -> rusqlite::Result<Option<Generation>> {
    let mut newest: Option<(Generation, i64)> = None;
    for generation in [Generation::V1, Generation::V2] {
        if !table_exists(connection, generation.session_table())? {
            continue;
        }
        for session in list_generation(connection, generation, Some(id), "")? {
            if newest.is_none_or(|(_, updated)| session.updated_ms >= updated) {
                newest = Some((generation, session.updated_ms));
            }
        }
    }
    Ok(newest.map(|(generation, _)| generation))
}

fn read_v2_messages(connection: &Connection, id: &str) -> AppResult<Vec<NativeMessage>> {
    let mut statement = connection.prepare(
        "SELECT type, time_created, time_updated, data FROM session_message WHERE session_id = ?1 ORDER BY seq, time_created, id",
    )?;
    let rows = statement.query_map([id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (kind, created, updated, raw) = row?;
        let Ok(data) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let message = v2_message(&kind, created, updated, &data);
        if !message.parts.is_empty()
            || matches!(message.role, NativeRole::System | NativeRole::Compaction)
        {
            messages.push(message);
        }
    }
    Ok(messages)
}

fn v2_message(kind: &str, created: i64, updated: i64, data: &Value) -> NativeMessage {
    let role = match kind {
        "synthetic" => NativeRole::System,
        other => NativeRole::from_name(other),
    };
    let parts = data
        .get("sessiongatorParts")
        .and_then(parts_from_json)
        .unwrap_or_else(|| match kind {
            "user" => non_empty_string(data.get("text"))
                .map(NativePart::Text)
                .into_iter()
                .chain(attachment_parts(data.get("files")))
                .collect(),
            "assistant" => data
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(assistant_content_part)
                .collect(),
            "system" | "synthetic" | "compaction" => non_empty_string(data.get("text"))
                .or_else(|| non_empty_string(data.get("summary")))
                .map(NativePart::Text)
                .into_iter()
                .collect(),
            "shell" => vec![NativePart::Text(format!(
                "$ {}\n{}",
                non_empty_string(data.get("command")).unwrap_or_default(),
                non_empty_string(data.get("output")).unwrap_or_default()
            ))],
            _ => Vec::new(),
        });
    NativeMessage {
        role,
        created_ms: created,
        updated_ms: Some(updated),
        parts,
        metadata: BTreeMap::new(),
    }
}

fn assistant_content_part(value: &Value) -> Option<NativePart> {
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => non_empty_string(value.get("text")).map(NativePart::Text),
        "reasoning" => non_empty_string(value.get("text")).map(|text| NativePart::Reasoning {
            text,
            metadata: value.get("providerMetadata").cloned(),
        }),
        "tool" => Some(NativePart::ToolCall {
            id: non_empty_string(value.get("id")).unwrap_or_else(|| generated_id("tool")),
            name: non_empty_string(value.get("name")).unwrap_or_else(|| "unknown".to_string()),
            input: value
                .pointer("/state/input")
                .cloned()
                .unwrap_or(Value::Null),
        }),
        "" => None,
        other => Some(NativePart::Raw {
            kind: other.to_string(),
            value: value.clone(),
        }),
    }
}

fn attachment_parts(value: Option<&Value>) -> impl Iterator<Item = NativePart> + '_ {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|file| NativePart::File {
            // OpenCode 2 writes `name`, `mime` and inline base64 `data`.
            name: non_empty_string(file.get("name"))
                .or_else(|| non_empty_string(file.get("filename"))),
            mime: non_empty_string(file.get("mime"))
                .or_else(|| non_empty_string(file.get("mediaType"))),
            url: non_empty_string(file.get("url")).or_else(|| {
                let data = non_empty_string(file.get("data"))?;
                let mime = non_empty_string(file.get("mime"))
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                Some(format!("data:{mime};base64,{data}"))
            }),
            path: None,
        })
}

fn read_v1_messages(connection: &Connection, id: &str) -> AppResult<Vec<NativeMessage>> {
    let mut statement = connection.prepare(
        "SELECT m.id, m.time_created, m.time_updated, m.data, p.data
         FROM message m LEFT JOIN part p ON p.message_id = m.id
         WHERE m.session_id = ?1
         ORDER BY m.time_created, m.id, p.time_created, p.id",
    )?;
    let rows = statement.query_map([id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut messages: Vec<NativeMessage> = Vec::new();
    let mut current_id = String::new();
    for row in rows {
        let (message_id, created, updated, message_raw, part_raw) = row?;
        if current_id != message_id {
            current_id = message_id;
            let data: Value = serde_json::from_str(&message_raw).unwrap_or(Value::Null);
            messages.push(NativeMessage {
                role: NativeRole::from_name(data.get("role").and_then(Value::as_str).unwrap_or("")),
                created_ms: created,
                updated_ms: Some(updated),
                parts: Vec::new(),
                metadata: BTreeMap::new(),
            });
        }
        let part = part_raw
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|part| v1_part(&part));
        if let (Some(part), Some(message)) = (part, messages.last_mut()) {
            message.parts.push(part);
        }
    }
    Ok(messages)
}

fn v1_part(part: &Value) -> Option<NativePart> {
    match part.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => non_empty_string(part.get("text")).map(NativePart::Text),
        "reasoning" => non_empty_string(part.get("text")).map(|text| NativePart::Reasoning {
            text,
            metadata: part.get("metadata").cloned(),
        }),
        "tool" => Some(NativePart::ToolCall {
            id: non_empty_string(part.get("callID")).unwrap_or_else(|| generated_id("tool")),
            name: non_empty_string(part.get("tool")).unwrap_or_else(|| "unknown".to_string()),
            input: part.pointer("/state/input").cloned().unwrap_or(Value::Null),
        }),
        "file" => Some(NativePart::File {
            name: non_empty_string(part.get("filename")),
            mime: non_empty_string(part.get("mime")),
            url: non_empty_string(part.get("url")),
            path: None,
        }),
        _ => None,
    }
}

/// The `model` column holds JSON like `{"id":"gpt-5.5","providerID":"openai"}`.
fn parse_model(raw: &str) -> Option<ModelRef> {
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => crate::native::model_ref_from_json(&value),
        Err(_) => Some(ModelRef {
            provider_id: None,
            id: raw.to_string(),
        }),
    }
}

fn write_session(connection: &Connection, request: &WriteRequest) -> AppResult<()> {
    let session = request.session;
    let exists = connection
        .query_row(
            "SELECT 1 FROM session_v2 WHERE id = ?1",
            [session.id.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        if !request.force {
            return Err(format!("target opencode session already exists: {}", session.id).into());
        }
        // The delete cascades to `session_message`.
        connection.execute(
            "DELETE FROM session_v2 WHERE id = ?1",
            [session.id.as_str()],
        )?;
    }
    let project_id = ensure_project(connection, &session.cwd, session.created_ms)?;
    // OpenCode 2 refuses to prompt a session with an agent it does not know
    // (AgentNotFoundError). NULL agent selects the default agent.
    connection.execute(
        "INSERT INTO session_v2
         (id, project_id, slug, directory, title, version, time_created, time_updated, agent, model, metadata,
          cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, 0, 0, 0, 0, 0, 0)",
        params![
            session.id,
            project_id,
            slug(&session.title),
            session.cwd,
            session.title,
            request.cli_version,
            session.created_ms,
            session.updated_ms,
            session.model.as_ref().map(|model| model_json(model).to_string()),
            request.provenance.to_string(),
        ],
    )?;
    let results = tool_results(&session.messages);
    for (index, message) in session.messages.iter().enumerate() {
        insert_message(
            connection,
            &session.id,
            session.model.as_ref(),
            message,
            index as i64 + 1,
            &results,
        )?;
    }
    bump_sequence(connection, &session.id, session.messages.len() as i64)?;
    Ok(())
}

/// OpenCode 2 numbers the next message after this sequence number.
fn bump_sequence(connection: &Connection, id: &str, seq: i64) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)
         ON CONFLICT(aggregate_id) DO UPDATE SET seq = excluded.seq, owner_id = NULL",
        params![id, seq],
    )?;
    Ok(())
}

fn ensure_project(connection: &Connection, cwd: &str, created_ms: i64) -> AppResult<String> {
    if let Some(id) = connection
        .query_row(
            "SELECT id FROM project WHERE worktree = ?1 LIMIT 1",
            [cwd],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    let id = generated_id("proj");
    let name = Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("imported");
    connection.execute(
        "INSERT INTO project (id, worktree, vcs, name, time_created, time_updated, sandboxes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, cwd, "git", name, created_ms, created_ms, "[]"],
    )?;
    if table_exists(connection, "project_directory")? {
        connection.execute(
            "INSERT OR IGNORE INTO project_directory (project_id, directory, type, time_created) VALUES (?1, ?2, ?3, ?4)",
            params![id, cwd, "main", created_ms],
        )?;
    }
    Ok(id)
}

/// Tool results by call id. OpenCode 2 keeps a call and its result in one
/// `tool` content item; Claude and Codex keep the result in a later message.
type ToolResults<'a> = HashMap<&'a str, (&'a Value, bool)>;

fn tool_results(messages: &[NativeMessage]) -> ToolResults<'_> {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            NativePart::ToolResult {
                id,
                content,
                is_error,
            } => Some((id.as_str(), (content, *is_error))),
            _ => None,
        })
        .collect()
}

fn insert_message(
    connection: &Connection,
    session_id: &str,
    model: Option<&ModelRef>,
    message: &NativeMessage,
    seq: i64,
    results: &ToolResults,
) -> rusqlite::Result<String> {
    let id = generated_id("msg");
    let kind = match message.role {
        NativeRole::Assistant => "assistant",
        NativeRole::System => "system",
        NativeRole::Shell => "shell",
        NativeRole::Compaction => "compaction",
        _ => "user",
    };
    connection.execute(
        "INSERT INTO session_message (id, session_id, type, time_created, time_updated, data, seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            session_id,
            kind,
            message.created_ms,
            message.updated_ms.unwrap_or(message.created_ms),
            message_data(model, message, results).to_string(),
            seq
        ],
    )?;
    Ok(id)
}

/// `session_message.data` in the shape OpenCode 2 decodes
/// (`@opencode/schema/session-message`). One row that does not decode makes
/// OpenCode 2 fail to load the whole session. `sessiongatorParts` keeps the
/// native parts for a lossless read back; OpenCode ignores unknown keys.
fn message_data(model: Option<&ModelRef>, message: &NativeMessage, results: &ToolResults) -> Value {
    let created = message.created_ms;
    let parts = parts_to_json(&message.parts);
    match message.role {
        NativeRole::Assistant => json!({
            "agent": "imported",
            "model": model.map(model_json).unwrap_or_else(|| json!({ "id": "imported", "providerID": "imported" })),
            "content": message.parts.iter().filter_map(|part| assistant_content(part, created, results)).collect::<Vec<_>>(),
            "sessiongatorParts": parts,
            "time": { "created": created, "completed": message.updated_ms.unwrap_or(created) },
            "cost": 0,
            "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
            "metadata": message.metadata,
        }),
        NativeRole::System => {
            json!({ "text": parts_text(&message.parts), "sessiongatorParts": parts, "time": { "created": created }, "metadata": message.metadata })
        }
        NativeRole::Shell => {
            let time = match message.updated_ms {
                Some(completed) => json!({ "created": created, "completed": completed }),
                None => json!({ "created": created }),
            };
            json!({ "shellID": generated_id("sh"), "command": parts_text(&message.parts), "status": "exited", "sessiongatorParts": parts, "time": time, "metadata": message.metadata })
        }
        NativeRole::Compaction => {
            json!({ "status": "completed", "reason": "manual", "summary": parts_text(&message.parts), "recent": "", "sessiongatorParts": parts, "time": { "created": created }, "metadata": message.metadata })
        }
        NativeRole::User | NativeRole::Unknown(_) => {
            let (files, notes) = attachments(&message.parts);
            let text = std::iter::once(parts_text(&message.parts))
                .chain(notes)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            json!({
                "text": text,
                "files": files,
                "agents": [],
                "sessiongatorParts": parts,
                "time": { "created": created },
                "metadata": message.metadata,
            })
        }
    }
}

/// A tool result has no item of its own: it travels in the state of its call.
fn assistant_content(part: &NativePart, created: i64, results: &ToolResults) -> Option<Value> {
    match part {
        NativePart::Text(text) => Some(json!({ "type": "text", "text": text })),
        NativePart::Reasoning { text, .. } => Some(json!({ "type": "reasoning", "text": text })),
        NativePart::ToolCall { id, name, input } => Some(json!({
            "type": "tool",
            "id": id,
            "name": name,
            "state": tool_state(input, results.get(id.as_str()).copied()),
            "time": { "created": created },
        })),
        _ => None,
    }
}

/// OpenCode 2 has no "pending" tool state. A call without an imported result
/// reads as interrupted, as in the OpenCode 2 migration of 1.x sessions.
fn tool_state(input: &Value, result: Option<(&Value, bool)>) -> Value {
    let input = match input {
        Value::Object(_) => input.clone(),
        Value::Null => json!({}),
        other => json!({ "value": other }),
    };
    match result {
        Some((content, false)) => json!({
            "status": "completed",
            "input": input,
            "content": [{ "type": "text", "text": content_to_string(content) }],
        }),
        Some((content, true)) => json!({
            "status": "error",
            "input": input,
            "error": { "type": "tool.execution", "message": content_to_string(content) },
        }),
        None => json!({
            "status": "error",
            "input": input,
            "error": { "type": "tool.interrupted", "message": "sessiongator imported no result for this tool call" },
        }),
    }
}

/// OpenCode 2 attachments carry inline base64 data. A base64 `data:` URL
/// becomes an attachment; any other file becomes a text note, as in the
/// OpenCode 2 migration. Limit: a non-base64 `data:` URL also becomes a
/// note; decode it here if imports with such files show up.
fn attachments(parts: &[NativePart]) -> (Vec<Value>, Vec<String>) {
    let mut files = Vec::new();
    let mut notes = Vec::new();
    for part in parts {
        let NativePart::File {
            name,
            mime,
            url,
            path,
        } = part
        else {
            continue;
        };
        let inline = url
            .as_deref()
            .and_then(|url| url.strip_prefix("data:"))
            .and_then(|rest| rest.split_once(','))
            .and_then(|(header, data)| {
                header
                    .strip_suffix(";base64")
                    .map(|header_mime| (header_mime, data))
            });
        match inline {
            Some((header_mime, data)) => {
                let mime = mime
                    .as_deref()
                    .filter(|mime| !mime.is_empty())
                    .or(Some(header_mime).filter(|mime| !mime.is_empty()))
                    .unwrap_or("application/octet-stream");
                let mut file =
                    json!({ "data": data, "mime": mime, "source": { "type": "inline" } });
                if let Some(name) = name {
                    file["name"] = json!(name);
                }
                files.push(file);
            }
            None => {
                let label = name
                    .as_deref()
                    .or(url.as_deref())
                    .or(path.as_deref())
                    .unwrap_or("file");
                let mime = mime.as_deref().unwrap_or("unknown type");
                notes.push(format!(
                    "[Attachment unavailable after import: {label} ({mime})]"
                ));
            }
        }
    }
    (files, notes)
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').chars().take(80).collect()
}

/// The tables of a fresh OpenCode 2.0.18 database that sessiongator touches.
#[cfg(test)]
pub(crate) fn create_test_v2_db(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    Connection::open(path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT, name TEXT,
                icon_url TEXT, icon_color TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                time_initialized INTEGER, sandboxes TEXT NOT NULL, commands TEXT, icon_url_override TEXT,
                time_active INTEGER DEFAULT 0 NOT NULL);
            CREATE TABLE session_v2 (id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
                workspace_id TEXT, parent_id TEXT, fork_session_id TEXT, fork_boundary TEXT,
                slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT,
                version TEXT NOT NULL, metadata TEXT, cost REAL DEFAULT 0 NOT NULL,
                tokens_input INTEGER DEFAULT 0 NOT NULL, tokens_output INTEGER DEFAULT 0 NOT NULL,
                tokens_reasoning INTEGER DEFAULT 0 NOT NULL, tokens_cache_read INTEGER DEFAULT 0 NOT NULL,
                tokens_cache_write INTEGER DEFAULT 0 NOT NULL, agent TEXT, model TEXT,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
            CREATE TABLE session_message (id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
                type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL);
            CREATE UNIQUE INDEX session_message_session_seq_idx ON session_message(session_id, seq);
            CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
            CREATE TABLE event (id TEXT PRIMARY KEY,
                aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE,
                seq INTEGER NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL);
            CREATE TABLE todo (session_id TEXT NOT NULL, content TEXT NOT NULL);",
        )
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::turns;
    use crate::stores::tests::{sample_session, temp_dir};

    const V1_SCHEMA: &str = r#"
        CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
            directory TEXT, title TEXT, agent TEXT, model TEXT, version TEXT,
            time_created INTEGER, time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT);
    "#;

    fn store_with(name: &str, sql: &str) -> OpencodeStore {
        let db = temp_dir(&format!("opencode-{name}")).join("opencode.db");
        create_test_v2_db(&db);
        Connection::open(&db).unwrap().execute_batch(sql).unwrap();
        OpencodeStore::new(db)
    }

    /// `ses_copied` exists in both generations (the migration copied it);
    /// `ses_new` exists only in V2. `v1_updated` sets the V1 copy's time.
    fn migrated_store(name: &str, v1_updated: i64) -> OpencodeStore {
        store_with(
            name,
            &format!(
                "{V1_SCHEMA}
                INSERT INTO session VALUES ('ses_copied', 'proj', NULL, '/w/old',
                    'Old title', 'build', NULL, '1.17.13', 100, {v1_updated});
                INSERT INTO message VALUES ('m1', 'ses_copied', 100, 100, '{{\"role\":\"user\"}}');
                INSERT INTO part VALUES ('p1', 'm1', 'ses_copied', 100, 100,
                    '{{\"type\":\"text\",\"text\":\"from v1\"}}');
                INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('proj', '/w', 1, 1, '[]');
                {}",
                r#"
                INSERT INTO session_v2 (id, project_id, directory, slug, title, version, agent, model, time_created, time_updated)
                    VALUES ('ses_copied', 'proj', '/w/old', 'old', 'Old title', '2.0.18', 'build', NULL, 100, 200);
                INSERT INTO session_message VALUES ('msg_a', 'ses_copied', 'user', 0, 100, 200,
                    '{"text":"from v2","time":{"created":100}}');
                INSERT INTO session_v2 (id, project_id, directory, slug, title, version, agent, model, time_created, time_updated)
                    VALUES ('ses_new', 'proj', '/w/new', 'new', NULL, '2.0.18', 'build', '{"id":"gpt-5.5","providerID":"openai"}', 300, 300);
                INSERT INTO session_message VALUES ('msg_b', 'ses_new', 'user', 0, 300, 300,
                    '{"text":"run the tests","time":{"created":300}}');
                INSERT INTO session_message VALUES ('msg_c', 'ses_new', 'system', 1, 301, 301,
                    '{"text":"not transcript","time":{"created":301}}');
                INSERT INTO session_message VALUES ('msg_d', 'ses_new', 'assistant', 2, 302, 900,
                    '{"content":[{"type":"reasoning","text":"hidden"},{"type":"text","text":"All green"},{"type":"tool","id":"t1","name":"shell"},{"type":"text","text":"Done"}]}');
                INSERT INTO todo VALUES ('ses_new', 'write tests');
                "#
            ),
        )
    }

    fn request<'a>(session: &'a NativeSession, provenance: &'a Value) -> WriteRequest<'a> {
        WriteRequest {
            session,
            cli_version: "2.0.28",
            provenance,
            force: false,
            backup: false,
        }
    }

    #[test]
    fn lists_each_session_once_with_newest_message_time_and_model() {
        let sessions = migrated_store("list", 150).list().unwrap();
        let mut ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["ses_copied", "ses_new"]);
        let new = sessions.iter().find(|s| s.id == "ses_new").unwrap();
        assert_eq!(new.updated_ms, 900);
        assert_eq!(new.message_count, 2);
        assert_eq!(new.title, "(no title)");
        assert_eq!(new.model.as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn transcript_reads_v2_text_in_seq_order() {
        let texts: Vec<(String, String)> = migrated_store("transcript", 150)
            .transcript("ses_new")
            .unwrap()
            .into_iter()
            .map(|turn| (turn.role, turn.text))
            .collect();
        assert_eq!(
            texts,
            [
                ("user".to_string(), "run the tests".to_string()),
                ("assistant".to_string(), "All green".to_string()),
                ("assistant".to_string(), "Done".to_string()),
            ]
        );
    }

    #[test]
    fn a_session_in_both_generations_reads_the_newer_copy() {
        let v2_newer = migrated_store("both-v2", 200);
        assert_eq!(
            v2_newer.transcript("ses_copied").unwrap()[0].text,
            "from v2"
        );
        let v1_newer = migrated_store("both-v1", 500);
        assert_eq!(
            v1_newer.transcript("ses_copied").unwrap()[0].text,
            "from v1"
        );
        let copied = v1_newer.read("ses_copied").unwrap();
        assert_eq!(
            copied.metadata["opencode_session_version"],
            json!("1.17.13")
        );
    }

    #[test]
    fn writes_a_v2_session_that_opencode_2_can_decode() {
        let store = store_with("write", "");
        let mut session = sample_session(Tool::Opencode, "ses_v2_import");
        session.messages.extend([
            NativeMessage {
                role: NativeRole::Assistant,
                created_ms: 1_783_000_000_500,
                updated_ms: None,
                parts: vec![
                    NativePart::Text("running it".to_string()),
                    NativePart::ToolCall {
                        id: "call_1".to_string(),
                        name: "Bash".to_string(),
                        input: json!({ "command": "ls" }),
                    },
                    NativePart::ToolCall {
                        id: "call_2".to_string(),
                        name: "Bash".to_string(),
                        input: Value::Null,
                    },
                ],
                metadata: BTreeMap::new(),
            },
            NativeMessage {
                role: NativeRole::User,
                created_ms: 1_783_000_000_600,
                updated_ms: None,
                parts: vec![NativePart::ToolResult {
                    id: "call_1".to_string(),
                    content: json!("a.txt"),
                    is_error: false,
                }],
                metadata: BTreeMap::new(),
            },
        ]);
        store.write(&request(&session, &json!({}))).unwrap();

        let connection = Connection::open(&store.db).unwrap();
        let agent: Option<String> = connection
            .query_row(
                "SELECT agent FROM session_v2 WHERE id = 'ses_v2_import'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            agent, None,
            "an unknown agent blocks prompting in OpenCode 2"
        );
        assert!(
            !table_exists(&connection, "session").unwrap(),
            "1.x tables make OpenCode 2 rerun its migration, which clears `event`"
        );
        let (watermark, last_seq): (i64, i64) = connection
            .query_row(
                "SELECT (SELECT seq FROM event_sequence WHERE aggregate_id = 'ses_v2_import'),
                        (SELECT max(seq) FROM session_message WHERE session_id = 'ses_v2_import')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(watermark, last_seq);
        let assistant: Value = connection
            .query_row(
                "SELECT data FROM session_message WHERE session_id = 'ses_v2_import' AND type = 'assistant'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|raw| serde_json::from_str(&raw).unwrap())
            .unwrap();
        let states: Vec<&str> = assistant["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["state"]["status"].as_str())
            .collect();
        assert_eq!(states, ["completed", "error"]);
        assert_eq!(assistant["content"][2]["state"]["input"], json!({}));

        let back = store.read("ses_v2_import").unwrap();
        assert_eq!(back.title, session.title);
        assert_eq!(back.messages.len(), 3);
        assert_eq!(turns(&back)[0].text, "hello");
        assert!(store.write(&request(&session, &json!({}))).is_err());
    }

    #[test]
    fn refuses_to_write_without_opencode_2_tables() {
        let db = temp_dir("opencode-v1-only").join("opencode.db");
        Connection::open(&db)
            .unwrap()
            .execute_batch(V1_SCHEMA)
            .unwrap();
        let session = sample_session(Tool::Opencode, "ses_x");
        let error = OpencodeStore::new(db)
            .write(&request(&session, &json!({})))
            .unwrap_err();
        assert!(
            error.to_string().contains("not an OpenCode 2 database"),
            "{error}"
        );
    }

    #[test]
    fn note_continues_the_message_sequence() {
        let store = migrated_store("note", 150);
        let footprint = store
            .append_note(
                "ses_new",
                &Note {
                    text: "moved",
                    cli_version: "2.0.28",
                },
            )
            .unwrap();
        assert_eq!(footprint.rows[0].table, "session_message");
        let connection = Connection::open(&store.db).unwrap();
        let (seq, watermark): (i64, i64) = connection
            .query_row(
                "SELECT m.seq, e.seq FROM session_message m JOIN event_sequence e ON e.aggregate_id = m.session_id WHERE m.id = ?1",
                [&footprint.rows[0].value],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((seq, watermark), (3, 3));
        assert_eq!(
            store.transcript("ses_new").unwrap().last().unwrap().text,
            "moved"
        );
    }

    #[test]
    fn footprint_covers_every_session_table_with_parents_last() {
        let footprint = migrated_store("footprint", 150)
            .footprint("ses_new")
            .unwrap();
        let tables: Vec<&str> = footprint
            .rows
            .iter()
            .map(|rows| rows.table.as_str())
            .collect();
        for table in [
            "message",
            "part",
            "session_message",
            "todo",
            "event",
            "event_sequence",
        ] {
            assert!(tables.contains(&table), "{table} missing from {tables:?}");
        }
        assert_eq!(&tables[tables.len() - 2..], ["session_v2", "session"]);
    }

    #[test]
    fn slug_is_stable_ascii() {
        assert_eq!(slug("Fix: Rate Limiter!"), "fix-rate-limiter");
    }
}
