//! Claude Code: one JSONL event log per session under
//! `<config root>/projects/<encoded cwd>/<id>.jsonl`.
//!
//! The project folder name is a lossy encoding of the working directory
//! (`/` replaced by `-`), so `cwd` always comes from event content.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use gator::AppResult;
use serde_json::{json, Value};

use super::{
    append_lines, env_path, home, write_atomic, Fingerprint, Footprint, LineSet, Note, Store,
    WriteRequest, Written,
};
use crate::model::{clean_title, Session, Tool};
use crate::native::{
    first_text_part, generated_id, generated_uuid, non_empty_string, title_from_text, ModelRef,
    NativeMessage, NativePart, NativeRole, NativeSession,
};
use crate::time::{iso_utc, now_ms, parse_iso_utc_ms};

/// Claude Code marks assistant messages that no model produced this way.
const SYNTHETIC_MODEL: &str = "<synthetic>";

pub fn default_root() -> PathBuf {
    env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude"))
}

pub struct ClaudeStore {
    root: PathBuf,
}

impl ClaudeStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(self.projects_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|project| project.path())
            .filter(|path| path.is_dir())
            .flat_map(|project| fs::read_dir(project).into_iter().flatten().flatten())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        files
    }

    fn find_file(&self, id: &str) -> AppResult<PathBuf> {
        fs::read_dir(self.projects_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|project| project.path().join(format!("{id}.jsonl")))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| format!("claude session {id} not found").into())
    }
}

impl Store for ClaudeStore {
    fn tool(&self) -> Tool {
        Tool::Claude
    }

    fn available(&self) -> bool {
        self.projects_dir().is_dir()
    }

    fn list(&self) -> AppResult<Vec<Session>> {
        Ok(self
            .session_files()
            .iter()
            .filter_map(|path| list_entry(path))
            .collect())
    }

    fn read(&self, id: &str) -> AppResult<NativeSession> {
        let path = self.find_file(id)?;
        read_session(&path, id)
    }

    fn fingerprint(&self) -> AppResult<Fingerprint> {
        let files = self.session_files().len();
        Ok(Fingerprint {
            store_version: None,
            schema: self.available().then(|| format!("projects-jsonl:{files}")),
        })
    }

    fn write(&self, request: &WriteRequest) -> AppResult<Written> {
        let session = request.session;
        let project = self.projects_dir().join(encode_project_dir(&session.cwd));
        fs::create_dir_all(&project)?;
        let path = project.join(format!("{}.jsonl", session.id));
        if path.exists() && !request.force {
            return Err(format!("target Claude session already exists: {}", path.display()).into());
        }
        let mut lines = vec![json!({
            "type": "ai-title",
            "aiTitle": session.title,
            "sessionId": session.id,
            "timestamp": iso_utc(session.created_ms),
            "version": request.cli_version,
            "sessiongator": request.provenance,
        })
        .to_string()];
        let model = session
            .model
            .as_ref()
            .map(|model| model.id.as_str())
            .unwrap_or("imported");
        let mut parent = Value::Null;
        for (index, message) in session.messages.iter().enumerate() {
            let uuid = format!("{}-{index:04}", session.id);
            let event = EventContext {
                session_id: &session.id,
                cwd: &session.cwd,
                version: request.cli_version,
                model,
            }
            .event(message, &uuid, parent);
            parent = json!(uuid);
            lines.push(event.to_string());
        }
        write_atomic(&path, &(lines.join("\n") + "\n"))?;
        Ok(Written {
            reference: path.display().to_string(),
            backup: None,
        })
    }

    fn footprint(&self, id: &str) -> AppResult<Footprint> {
        let file = self.find_file(id)?;
        let mut paths = vec![file.clone()];
        // Subagent transcripts and tool output sidecars.
        paths.push(file.with_extension(""));
        paths.push(self.root.join("session-env").join(id));
        paths.push(self.root.join("file-history").join(id));
        paths.retain(|path| path.exists());
        Ok(Footprint {
            paths,
            ..Footprint::default()
        })
    }

    fn append_note(&self, id: &str, note: &Note) -> AppResult<Footprint> {
        let path = self.find_file(id)?;
        let session = read_session(&path, id)?;
        let parent = last_event_uuid(&path)?.map_or(Value::Null, Value::String);
        let message = NativeMessage::text(NativeRole::Assistant, now_ms(), note.text.to_string());
        let line = EventContext {
            session_id: id,
            cwd: &session.cwd,
            version: note.cli_version,
            model: SYNTHETIC_MODEL,
        }
        .event(&message, &generated_uuid(), parent)
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

/// Scan one JSONL file for list metadata. Corrupt lines are skipped; a
/// session is never dropped for one bad event.
fn list_entry(path: &Path) -> Option<Session> {
    let id = path.file_stem()?.to_str()?.to_string();
    let file = fs::File::open(path).ok()?;
    let mut cwd = None;
    let mut branch = None;
    let mut ai_title = None;
    let mut first_user = None;
    let mut model = None;
    let mut created_ms = None;
    let mut updated_ms = None;
    let mut count: u32 = 0;

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = non_empty_string(event.get("cwd"));
        }
        if branch.is_none() {
            branch = non_empty_string(event.get("gitBranch"));
        }
        if let Some(timestamp) = event_time(&event) {
            created_ms.get_or_insert(timestamp);
            updated_ms = Some(timestamp);
        }
        match event.get("type").and_then(Value::as_str) {
            Some("ai-title") => ai_title = non_empty_string(event.get("aiTitle")).or(ai_title),
            Some("user") => {
                count += 1;
                if first_user.is_none() {
                    first_user = first_text_part(&message_parts(event.get("message")));
                }
            }
            Some("assistant") => {
                count += 1;
                if let Some(value) = event
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .filter(|value| *value != SYNTHETIC_MODEL)
                {
                    model = Some(value.to_string());
                }
            }
            _ => {}
        }
    }

    let title = ai_title
        .map(|text| clean_title(&text))
        .or_else(|| first_user.map(|text| title_from_text(&text)))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "(no title)".to_string());
    let updated = updated_ms.or(created_ms).unwrap_or(0);
    Some(Session {
        tool: Tool::Claude,
        id,
        title,
        cwd: cwd.unwrap_or_default(),
        created_ms: created_ms.unwrap_or(updated),
        updated_ms: updated,
        message_count: count,
        model,
        source_ref: path.display().to_string(),
        extras: branch
            .map(|value| vec![("branch".to_string(), value)])
            .unwrap_or_default(),
    })
}

fn read_session(path: &Path, id: &str) -> AppResult<NativeSession> {
    let file = fs::File::open(path)?;
    let mut title = None;
    let mut first_user = None;
    let mut cwd = None;
    let mut model = None;
    let mut created_ms = None;
    let mut updated_ms = None;
    let mut messages = Vec::new();
    let mut metadata = BTreeMap::new();
    metadata.insert("source_ref".to_string(), json!(path.display().to_string()));

    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = non_empty_string(event.get("cwd"));
        }
        if let Some(branch) = non_empty_string(event.get("gitBranch")) {
            metadata
                .entry("gitBranch".to_string())
                .or_insert(json!(branch));
        }
        if let Some(version) = non_empty_string(event.get("version")) {
            metadata
                .entry("claude_version".to_string())
                .or_insert(json!(version));
        }
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type == "ai-title" {
            title = non_empty_string(event.get("aiTitle")).or(title);
            continue;
        }
        let role = match event_type {
            "user" => NativeRole::User,
            "assistant" => NativeRole::Assistant,
            "system" => NativeRole::System,
            _ => continue,
        };
        let timestamp = event_time(&event).unwrap_or_else(now_ms);
        created_ms.get_or_insert(timestamp);
        updated_ms = Some(timestamp);
        let parts = message_parts(event.get("message"));
        if role == NativeRole::Assistant {
            if let Some(value) = event
                .pointer("/message/model")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && *value != SYNTHETIC_MODEL)
            {
                model = Some(ModelRef {
                    provider_id: Some("anthropic".to_string()),
                    id: value.to_string(),
                });
            }
        }
        if first_user.is_none() && role == NativeRole::User {
            first_user = first_text_part(&parts);
        }
        let mut message_metadata = BTreeMap::new();
        if let Some(uuid) = non_empty_string(event.get("uuid")) {
            message_metadata.insert("claude_uuid".to_string(), json!(uuid));
        }
        if let Some(parent) = non_empty_string(event.get("parentUuid")) {
            message_metadata.insert("claude_parent_uuid".to_string(), json!(parent));
        }
        messages.push(NativeMessage {
            role,
            created_ms: timestamp,
            updated_ms: None,
            parts,
            metadata: message_metadata,
        });
    }

    let updated = updated_ms.or(created_ms).unwrap_or_else(now_ms);
    Ok(NativeSession {
        tool: Tool::Claude,
        id: id.to_string(),
        title: title
            .or_else(|| first_user.map(|text| title_from_text(&text)))
            .unwrap_or_else(|| "(imported session)".to_string()),
        cwd: cwd.unwrap_or_default(),
        created_ms: created_ms.unwrap_or(updated),
        updated_ms: updated,
        model,
        messages,
        metadata,
    })
}

fn event_time(event: &Value) -> Option<i64> {
    event
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso_utc_ms)
}

fn last_event_uuid(path: &Path) -> AppResult<Option<String>> {
    let content = fs::read_to_string(path)?;
    Ok(content.lines().rev().find_map(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|event| non_empty_string(event.get("uuid")))
    }))
}

fn message_parts(message: Option<&Value>) -> Vec<NativePart> {
    let Some(message) = message else {
        return Vec::new();
    };
    if let Some(text) = message.as_str().filter(|text| !text.is_empty()) {
        return vec![NativePart::Text(text.to_string())];
    }
    let Some(content) = message.get("content") else {
        return Vec::new();
    };
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        return vec![NativePart::Text(text.to_string())];
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(content_block)
        .collect()
}

fn content_block(block: &Value) -> Option<NativePart> {
    match block.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => non_empty_string(block.get("text")).map(NativePart::Text),
        "thinking" => non_empty_string(block.get("thinking")).map(|text| NativePart::Reasoning {
            text,
            metadata: block
                .get("signature")
                .cloned()
                .map(|signature| json!({ "signature": signature })),
        }),
        "tool_use" => Some(NativePart::ToolCall {
            id: non_empty_string(block.get("id")).unwrap_or_else(|| generated_id("tool")),
            name: non_empty_string(block.get("name")).unwrap_or_else(|| "unknown".to_string()),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => Some(NativePart::ToolResult {
            id: non_empty_string(block.get("tool_use_id")).unwrap_or_else(|| generated_id("tool")),
            content: block.get("content").cloned().unwrap_or(Value::Null),
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "file" => Some(NativePart::File {
            name: non_empty_string(block.get("filename"))
                .or_else(|| non_empty_string(block.get("name"))),
            mime: non_empty_string(block.get("mime"))
                .or_else(|| non_empty_string(block.get("mediaType"))),
            url: non_empty_string(block.get("url")),
            path: non_empty_string(block.get("path")),
        }),
        "" => None,
        other => Some(NativePart::Raw {
            kind: other.to_string(),
            value: block.clone(),
        }),
    }
}

/// Fields shared by every event of one written session.
struct EventContext<'a> {
    session_id: &'a str,
    cwd: &'a str,
    version: &'a str,
    model: &'a str,
}

impl EventContext<'_> {
    fn event(&self, message: &NativeMessage, uuid: &str, parent: Value) -> Value {
        let role = match message.role {
            NativeRole::Assistant => "assistant",
            NativeRole::System => "system",
            _ => "user",
        };
        let block: fn(&NativePart) -> Option<Value> = if role == "assistant" {
            assistant_block
        } else {
            user_block
        };
        json!({
            "type": role,
            "sessionId": self.session_id,
            "uuid": uuid,
            "parentUuid": parent,
            "cwd": self.cwd,
            "timestamp": iso_utc(message.created_ms),
            "version": self.version,
            "message": {
                "role": role,
                "model": self.model,
                "content": message.parts.iter().filter_map(block).collect::<Vec<_>>(),
            },
        })
    }
}

fn assistant_block(part: &NativePart) -> Option<Value> {
    match part {
        NativePart::Text(text) => Some(json!({ "type": "text", "text": text })),
        NativePart::Reasoning { text, metadata } => {
            let mut value = json!({ "type": "thinking", "thinking": text });
            if let Some(signature) = metadata
                .as_ref()
                .and_then(|metadata| metadata.get("signature"))
            {
                value["signature"] = signature.clone();
            }
            Some(value)
        }
        NativePart::ToolCall { id, name, input } => Some(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        _ => None,
    }
}

fn user_block(part: &NativePart) -> Option<Value> {
    match part {
        NativePart::Text(text) => Some(json!({ "type": "text", "text": text })),
        NativePart::ToolResult {
            id,
            content,
            is_error,
        } => Some(json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
            "is_error": is_error,
        })),
        NativePart::File {
            name,
            mime,
            url,
            path,
        } => Some(json!({
            "type": "file",
            "filename": name,
            "mime": mime,
            "url": url,
            "path": path,
        })),
        _ => None,
    }
}

/// Claude Code's lossy project folder name for `cwd`.
pub fn encode_project_dir(cwd: &str) -> String {
    let encoded = cwd.replace('/', "-");
    if encoded.is_empty() {
        "-".to_string()
    } else {
        encoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::turns;
    use crate::stores::tests::{sample_session, temp_dir};

    const ID: &str = "11111111-2222-3333-4444-555555555555";

    fn fixture_store(name: &str) -> ClaudeStore {
        let root = temp_dir(&format!("claude-{name}"));
        let project = root.join("projects").join("-Users-me-Github-demo");
        fs::create_dir_all(&project).unwrap();
        let events = [
            r#"{"type":"user","uuid":"u1","cwd":"/Users/me/Github/demo","gitBranch":"main","timestamp":"2026-06-29T16:11:35.000Z","sessionId":"S","message":{"role":"user","content":"please fix the rate limiter bug"}}"#,
            r#"{"type":"assistant","uuid":"u2","cwd":"/Users/me/Github/demo","timestamp":"2026-06-29T16:12:00.000Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"thinking","thinking":"secret reasoning"},{"type":"text","text":"Fixed the rate limiter."}]}}"#,
            "this line is corrupt json",
            r#"{"type":"user","uuid":"u3","timestamp":"2026-06-29T16:12:30.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"TOOLNOISE"}]}}"#,
            r#"{"type":"ai-title","aiTitle":"Fix the rate limiter","sessionId":"S"}"#,
        ];
        fs::write(project.join(format!("{ID}.jsonl")), events.join("\n")).unwrap();
        ClaudeStore::new(root)
    }

    #[test]
    fn lists_metadata_from_content_not_path() {
        let sessions = fixture_store("list").list().unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.title, "Fix the rate limiter");
        assert_eq!(session.cwd, "/Users/me/Github/demo");
        assert_eq!(session.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(session.message_count, 3);
        assert_eq!(session.extras, [("branch".to_string(), "main".to_string())]);
        assert!(session.created_ms > 0 && session.updated_ms >= session.created_ms);
    }

    #[test]
    fn transcript_excludes_thinking_and_tool_results() {
        let turns = fixture_store("transcript").transcript(ID).unwrap();
        let texts: Vec<&str> = turns.iter().map(|turn| turn.text.as_str()).collect();
        assert_eq!(
            texts,
            ["please fix the rate limiter bug", "Fixed the rate limiter."]
        );
    }

    #[test]
    fn reads_reasoning_and_tool_parts_for_conversion() {
        let session = fixture_store("parts").read(ID).unwrap();
        assert!(matches!(
            session.messages[1].parts[0],
            NativePart::Reasoning { .. }
        ));
        assert!(matches!(
            session.messages[2].parts[0],
            NativePart::ToolResult { .. }
        ));
        assert_eq!(session.cwd, "/Users/me/Github/demo");
    }

    #[test]
    fn writes_a_session_that_reads_back() {
        let store = ClaudeStore::new(temp_dir("claude-write"));
        let session = sample_session(Tool::Claude, ID);
        let written = store
            .write(&WriteRequest {
                session: &session,
                cli_version: "2.1.299",
                provenance: &json!({}),
                force: false,
                backup: false,
            })
            .unwrap();
        assert!(written.reference.ends_with(&format!("{ID}.jsonl")));
        let back = store.read(ID).unwrap();
        assert_eq!(back.title, session.title);
        assert_eq!(back.cwd, session.cwd);
        assert_eq!(turns(&back), turns(&session));
        assert!(store
            .write(&WriteRequest {
                session: &session,
                cli_version: "2.1.299",
                provenance: &json!({}),
                force: false,
                backup: false,
            })
            .is_err());
    }

    #[test]
    fn note_appends_one_chained_assistant_line() {
        let store = fixture_store("note");
        let footprint = store
            .append_note(
                ID,
                &Note {
                    text: "moved",
                    cli_version: "2.1.299",
                },
            )
            .unwrap();
        let line: Value = serde_json::from_str(&footprint.lines[0].lines[0]).unwrap();
        assert_eq!(line["parentUuid"], "u3");
        assert_eq!(line["message"]["model"], SYNTHETIC_MODEL);
        let turns = store.transcript(ID).unwrap();
        assert_eq!(turns.last().unwrap().text, "moved");
        // The note does not become the session model.
        assert_eq!(
            store.list().unwrap()[0].model.as_deref(),
            Some("claude-opus-4-8")
        );
    }

    #[test]
    fn footprint_holds_the_log_and_existing_sidecars() {
        let store = fixture_store("footprint");
        fs::create_dir_all(store.root.join("session-env").join(ID)).unwrap();
        let footprint = store.footprint(ID).unwrap();
        assert_eq!(footprint.paths.len(), 2);
        assert!(footprint.paths[0].ends_with(format!("{ID}.jsonl")));
    }

    #[test]
    fn project_dir_matches_the_lossy_claude_shape() {
        assert_eq!(encode_project_dir("/Users/me/demo"), "-Users-me-demo");
    }
}
