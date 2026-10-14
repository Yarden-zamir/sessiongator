//! Codex CLI: one rollout JSONL per session under
//! `<root>/sessions/YYYY/MM/DD/rollout-<timestamp>-<id>.jsonl`, plus a
//! shared `<root>/session_index.jsonl` with one line per session title.

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
    first_text_part, generated_id, model_ref_from_json, non_empty_string, parts_text,
    title_from_text, NativeMessage, NativePart, NativeRole, NativeSession,
};
use crate::time::{civil_date, iso_utc, now_ms, parse_iso_utc_ms};

/// `$CODEX_HOME` first, then `~/.codex`. New sessions go to the first root.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = env_path("CODEX_HOME").into_iter().collect();
    let default = home().join(".codex");
    if !roots.contains(&default) {
        roots.push(default);
    }
    roots
}

pub struct CodexStore {
    roots: Vec<PathBuf>,
}

impl CodexStore {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        assert!(!roots.is_empty(), "a Codex store needs at least one root");
        Self { roots }
    }

    fn write_root(&self) -> &Path {
        &self.roots[0]
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for root in &self.roots {
            collect_jsonl_files(&root.join("sessions"), &mut files);
            collect_jsonl_files(&root.join("archived_sessions"), &mut files);
        }
        files.sort();
        files.dedup();
        files
    }

    fn find_file(&self, id: &str) -> AppResult<PathBuf> {
        let files = self.session_files();
        files
            .iter()
            .find(|path| id_from_path(path).as_deref() == Some(id))
            .or_else(|| files.iter().find(|path| head_mentions(path, id)))
            .cloned()
            .ok_or_else(|| format!("codex session {id} not found").into())
    }
}

impl Store for CodexStore {
    fn tool(&self) -> Tool {
        Tool::Codex
    }

    fn available(&self) -> bool {
        self.roots
            .iter()
            .any(|root| root.join("sessions").is_dir() || root.join("archived_sessions").is_dir())
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
        let mut session = read_rollout(&path)?;
        session.id = id.to_string();
        Ok(session)
    }

    fn fingerprint(&self) -> AppResult<Fingerprint> {
        let files = self.session_files().len();
        let index = self
            .roots
            .iter()
            .any(|root| root.join("session_index.jsonl").is_file());
        Ok(Fingerprint {
            store_version: None,
            schema: Some(format!("codex:sessions-jsonl:{files};index:{index}")),
        })
    }

    fn write(&self, request: &WriteRequest) -> AppResult<Written> {
        let session = request.session;
        let root = self.write_root();
        let (year, month, day) = civil_date(session.created_ms);
        let dir = root
            .join("sessions")
            .join(format!("{year:04}"))
            .join(format!("{month:02}"))
            .join(format!("{day:02}"));
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!(
            "rollout-{}-{}.jsonl",
            filename_timestamp(session.created_ms),
            session.id
        ));
        if path.exists() && !request.force {
            return Err(format!("target Codex session already exists: {}", path.display()).into());
        }
        let meta = json!({
            "timestamp": iso_utc(session.created_ms),
            "type": "session_meta",
            "payload": {
                "id": session.id,
                "session_id": session.id,
                "timestamp": iso_utc(session.created_ms),
                "cwd": session.cwd,
                "originator": "sessiongator",
                "cli_version": request.cli_version,
                "model_provider": session.model.as_ref().and_then(|model| model.provider_id.as_deref()).unwrap_or("imported"),
                "source": "cli",
                "history_mode": "legacy",
                "sessiongator": request.provenance,
                "title": session.title,
            },
        });
        let mut lines = vec![meta.to_string()];
        lines.extend(
            session
                .messages
                .iter()
                .flat_map(message_lines)
                .map(|line| line.to_string()),
        );
        write_atomic(&path, &(lines.join("\n") + "\n"))?;
        let index = root.join("session_index.jsonl");
        if !index.exists() {
            fs::write(&index, "")?;
        }
        append_lines(
            &index,
            &[json!({ "id": session.id, "thread_name": session.title, "updated_at": iso_utc(session.updated_ms) })
                .to_string()],
        )?;
        Ok(Written {
            reference: path.display().to_string(),
            backup: None,
        })
    }

    fn footprint(&self, id: &str) -> AppResult<Footprint> {
        let path = self.find_file(id)?;
        let mut lines = Vec::new();
        for root in &self.roots {
            let index = root.join("session_index.jsonl");
            let Ok(content) = fs::read_to_string(&index) else {
                continue;
            };
            let matching: Vec<String> = content
                .lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line)
                        .is_ok_and(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
                })
                .map(str::to_string)
                .collect();
            if !matching.is_empty() {
                lines.push(LineSet {
                    file: index,
                    lines: matching,
                });
            }
        }
        Ok(Footprint {
            paths: vec![path],
            lines,
            ..Footprint::default()
        })
    }

    fn append_note(&self, id: &str, note: &Note) -> AppResult<Footprint> {
        let path = self.find_file(id)?;
        let message = NativeMessage::text(NativeRole::Assistant, now_ms(), note.text.to_string());
        let lines: Vec<String> = message_lines(&message)
            .into_iter()
            .map(|line| line.to_string())
            .collect();
        append_lines(&path, &lines)?;
        Ok(Footprint {
            lines: vec![LineSet { file: path, lines }],
            ..Footprint::default()
        })
    }
}

fn collect_jsonl_files(path: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
}

/// The id after `rollout-YYYY-MM-DDTHH-MM-SS-`.
fn id_from_path(path: &Path) -> Option<String> {
    const TIMESTAMP: usize = "2026-07-03T10-00-01-".len();
    let rest = path.file_stem()?.to_str()?.strip_prefix("rollout-")?;
    Some(rest.get(TIMESTAMP..).unwrap_or(rest).to_string())
}

fn head_mentions(path: &Path, id: &str) -> bool {
    fs::File::open(path).is_ok_and(|file| {
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .take(5)
            .any(|line| line.contains(id))
    })
}

fn filename_timestamp(epoch_ms: i64) -> String {
    let iso = iso_utc(epoch_ms);
    iso.split('.').next().unwrap_or(&iso).replace(':', "-")
}

/// Which copy of the conversation a rollout line belongs to. Current Codex
/// writes every visible message twice: as a `response_item` and as an
/// `event_msg`. The `event_msg` copy is the visible transcript when present.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Response,
    Event,
    /// Tool calls, tool output, and reasoning: only in `response_item`.
    Auxiliary,
}

/// Rollout content with each message tagged by its copy.
struct Rollout {
    meta: BTreeMap<String, Value>,
    title: Option<String>,
    cwd: Option<String>,
    model: Option<crate::native::ModelRef>,
    created_ms: Option<i64>,
    updated_ms: Option<i64>,
    messages: Vec<(NativeMessage, Origin)>,
}

impl Rollout {
    fn parse(path: &Path) -> AppResult<Self> {
        let mut rollout = Rollout {
            meta: BTreeMap::new(),
            title: None,
            cwd: None,
            model: None,
            created_ms: None,
            updated_ms: None,
            messages: Vec::new(),
        };
        for line in BufReader::new(fs::File::open(path)?).lines() {
            let line = line?;
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(meta) = meta_value(&event) {
                rollout.title = non_empty_string(meta.get("title")).or(rollout.title);
                rollout.cwd = non_empty_string(meta.get("cwd")).or(rollout.cwd);
                if let Some(version) = non_empty_string(meta.get("cli_version")) {
                    rollout
                        .meta
                        .insert("codex_cli_version".to_string(), json!(version));
                }
                if let Some(provider) = non_empty_string(meta.get("model_provider")) {
                    rollout
                        .meta
                        .insert("codex_model_provider".to_string(), json!(provider));
                }
                if let Some(timestamp) = non_empty_string(meta.get("timestamp"))
                    .and_then(|value| parse_iso_utc_ms(&value))
                {
                    rollout.created_ms.get_or_insert(timestamp);
                    rollout.updated_ms.get_or_insert(timestamp);
                }
                continue;
            }
            if rollout.model.is_none() {
                rollout.model = event
                    .pointer("/payload/model")
                    .or_else(|| event.get("model"))
                    .and_then(model_ref_from_json);
            }
            if let Some((message, copy)) = message_from_event(&event) {
                rollout.created_ms.get_or_insert(message.created_ms);
                rollout.updated_ms = Some(message.created_ms);
                rollout.messages.push((message, copy));
            }
        }
        Ok(rollout)
    }

    /// Visible messages from one copy only: `event_msg` when present.
    fn visible(&self) -> impl Iterator<Item = &NativeMessage> {
        let copy = if self.messages.iter().any(|(_, copy)| *copy == Origin::Event) {
            Origin::Event
        } else {
            Origin::Response
        };
        self.messages
            .iter()
            .filter(move |(_, kept)| *kept == copy)
            .map(|(message, _)| message)
    }
}

fn list_entry(path: &Path) -> Option<Session> {
    let id = id_from_path(path)?;
    let rollout = Rollout::parse(path).ok()?;
    let first_user = rollout
        .visible()
        .find(|message| message.role == NativeRole::User)
        .and_then(|message| first_text_part(&message.parts));
    let updated = rollout.updated_ms.or(rollout.created_ms).unwrap_or(0);
    Some(Session {
        tool: Tool::Codex,
        id,
        title: rollout
            .title
            .as_deref()
            .map(clean_title)
            .or_else(|| first_user.map(|text| title_from_text(&text)))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "(no title)".to_string()),
        cwd: rollout.cwd.clone().unwrap_or_default(),
        created_ms: rollout.created_ms.unwrap_or(updated),
        updated_ms: updated,
        message_count: rollout.visible().count() as u32,
        model: rollout
            .model
            .as_ref()
            .map(|model| model.id.clone())
            .or_else(|| {
                rollout
                    .meta
                    .get("codex_model_provider")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        source_ref: path.display().to_string(),
        extras: Vec::new(),
    })
}

fn read_rollout(path: &Path) -> AppResult<NativeSession> {
    let rollout = Rollout::parse(path)?;
    let mut messages: Vec<NativeMessage> = rollout.visible().cloned().collect();
    messages.extend(
        rollout
            .messages
            .iter()
            .filter(|(_, copy)| *copy == Origin::Auxiliary)
            .map(|(message, _)| message.clone()),
    );
    messages.sort_by_key(|message| message.created_ms);
    let first_user = messages
        .iter()
        .find(|message| message.role == NativeRole::User)
        .and_then(|message| first_text_part(&message.parts));
    let updated = rollout
        .updated_ms
        .or(rollout.created_ms)
        .unwrap_or_else(now_ms);
    let mut metadata = rollout.meta;
    metadata.insert("source_ref".to_string(), json!(path.display().to_string()));
    Ok(NativeSession {
        tool: Tool::Codex,
        id: id_from_path(path).unwrap_or_default(),
        title: rollout
            .title
            .or_else(|| first_user.map(|text| title_from_text(&text)))
            .unwrap_or_else(|| "(imported session)".to_string()),
        cwd: rollout.cwd.unwrap_or_default(),
        created_ms: rollout.created_ms.unwrap_or(updated),
        updated_ms: updated,
        model: rollout.model,
        messages,
        metadata,
    })
}

fn meta_value(event: &Value) -> Option<&Value> {
    if event.get("type").and_then(Value::as_str) == Some("session_meta") {
        return Some(event.get("payload").unwrap_or(event));
    }
    let item = event.get("item").unwrap_or(event);
    match item.get("type").and_then(Value::as_str) {
        Some("session_meta" | "SessionMeta" | "session_meta_line") => {
            Some(item.get("meta").unwrap_or(item))
        }
        _ => None,
    }
}

fn event_time(event: &Value) -> Option<i64> {
    event.get("created_ms").and_then(Value::as_i64).or_else(|| {
        non_empty_string(event.get("timestamp")).and_then(|value| parse_iso_utc_ms(&value))
    })
}

fn message_from_event(event: &Value) -> Option<(NativeMessage, Origin)> {
    let timestamp = event_time(event).unwrap_or_else(now_ms);
    let message = |role, parts| NativeMessage {
        role,
        created_ms: timestamp,
        updated_ms: None,
        parts,
        metadata: BTreeMap::new(),
    };
    match event.get("type").and_then(Value::as_str) {
        Some("response_item") => response_item(event.get("payload")?, timestamp),
        Some("event_msg") => {
            let payload = event.get("payload")?;
            let role = match payload.get("type").and_then(Value::as_str)? {
                "user_message" => NativeRole::User,
                "agent_message" => NativeRole::Assistant,
                _ => return None,
            };
            let text = non_empty_string(payload.get("message"))?;
            Some((message(role, vec![NativePart::Text(text)]), Origin::Event))
        }
        _ => None,
    }
}

fn response_item(payload: &Value, timestamp: i64) -> Option<(NativeMessage, Origin)> {
    let message = |role, part| NativeMessage {
        role,
        created_ms: timestamp,
        updated_ms: None,
        parts: vec![part],
        metadata: BTreeMap::new(),
    };
    match payload.get("type").and_then(Value::as_str)? {
        "message" => {
            let role = match payload.get("role").and_then(Value::as_str)? {
                "user" => NativeRole::User,
                "assistant" => NativeRole::Assistant,
                _ => return None,
            };
            let parts: Vec<NativePart> = payload
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|part| {
                    matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("input_text" | "output_text" | "text")
                    )
                })
                .filter_map(|part| non_empty_string(part.get("text")).map(NativePart::Text))
                .collect();
            (!parts.is_empty()).then(|| {
                (
                    NativeMessage {
                        role,
                        created_ms: timestamp,
                        updated_ms: None,
                        parts,
                        metadata: BTreeMap::new(),
                    },
                    Origin::Response,
                )
            })
        }
        "function_call" => Some((
            message(
                NativeRole::Assistant,
                NativePart::ToolCall {
                    id: non_empty_string(payload.get("call_id"))
                        .unwrap_or_else(|| generated_id("tool")),
                    name: non_empty_string(payload.get("name"))
                        .unwrap_or_else(|| "unknown".to_string()),
                    input: match payload.get("arguments") {
                        Some(Value::String(raw)) => {
                            serde_json::from_str(raw).unwrap_or_else(|_| json!(raw))
                        }
                        Some(other) => other.clone(),
                        None => Value::Null,
                    },
                },
            ),
            Origin::Auxiliary,
        )),
        "function_call_output" => Some((
            message(
                NativeRole::User,
                NativePart::ToolResult {
                    id: non_empty_string(payload.get("call_id"))
                        .unwrap_or_else(|| generated_id("tool")),
                    content: payload.get("output").cloned().unwrap_or(Value::Null),
                    is_error: false,
                },
            ),
            Origin::Auxiliary,
        )),
        "reasoning" => Some((
            message(
                NativeRole::Assistant,
                NativePart::Reasoning {
                    text: reasoning_text(payload)?,
                    metadata: payload
                        .get("summary")
                        .cloned()
                        .map(|summary| json!({ "summary": summary })),
                },
            ),
            Origin::Auxiliary,
        )),
        "web_search_call" => Some((
            message(
                NativeRole::Assistant,
                NativePart::ToolCall {
                    id: generated_id("tool"),
                    name: "web_search".to_string(),
                    input: payload.get("action").cloned().unwrap_or(Value::Null),
                },
            ),
            Origin::Auxiliary,
        )),
        other => Some((
            message(
                NativeRole::Unknown(other.to_string()),
                NativePart::Raw {
                    kind: other.to_string(),
                    value: payload.clone(),
                },
            ),
            Origin::Auxiliary,
        )),
    }
}

/// Reasoning summary text, else reasoning content text.
fn reasoning_text(payload: &Value) -> Option<String> {
    ["summary", "content"].iter().find_map(|key| {
        let text = payload
            .get(*key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| non_empty_string(item.get("text")))
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    })
}

/// The `response_item` and `event_msg` lines Codex writes for one visible
/// message. Other roles and parts have no Codex text form and are skipped.
fn message_lines(message: &NativeMessage) -> Vec<Value> {
    let (role, content_type, event) = match message.role {
        NativeRole::User => ("user", "input_text", "user_message"),
        NativeRole::Assistant => ("assistant", "output_text", "agent_message"),
        _ => return Vec::new(),
    };
    let text = parts_text(&message.parts);
    if text.trim().is_empty() {
        return Vec::new();
    }
    let timestamp = iso_utc(message.created_ms);
    let payload = if message.role == NativeRole::User {
        json!({ "type": event, "message": text, "images": [], "local_images": [], "text_elements": [] })
    } else {
        json!({ "type": event, "message": text })
    };
    vec![
        json!({
            "timestamp": timestamp,
            "type": "response_item",
            "payload": { "type": "message", "role": role, "content": [{ "type": content_type, "text": text }] },
        }),
        json!({ "timestamp": timestamp, "type": "event_msg", "payload": payload }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::turns;
    use crate::stores::tests::{sample_session, temp_dir};

    const ID: &str = "22222222-3333-4444-8555-666666666666";

    fn native_store(name: &str) -> CodexStore {
        let root = temp_dir(&format!("codex-{name}"));
        let dir = root.join("sessions/2026/07/03");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("rollout-2026-07-03T10-00-01-{ID}.jsonl")),
            [
                format!(r#"{{"type":"session_meta","timestamp":"2026-07-03T10:00:01.000Z","payload":{{"id":"{ID}","cwd":"/tmp/native-codex","timestamp":"2026-07-03T10:00:01.000Z","cli_version":"0.143.0","model_provider":"openai"}}}}"#),
                r#"{"type":"event_msg","timestamp":"2026-07-03T10:00:01.100Z","payload":{"type":"user_message","message":"visible user"}}"#.to_string(),
                r#"{"type":"response_item","timestamp":"2026-07-03T10:00:01.100Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"native user"}]}}"#.to_string(),
                r#"{"type":"event_msg","timestamp":"2026-07-03T10:00:02.100Z","payload":{"type":"agent_message","message":"visible assistant"}}"#.to_string(),
                r#"{"type":"response_item","timestamp":"2026-07-03T10:00:02.100Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"native assistant"}]}}"#.to_string(),
                r#"{"type":"response_item","timestamp":"2026-07-03T10:00:03.100Z","payload":{"type":"function_call","call_id":"call_1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#.to_string(),
                r#"{"type":"response_item","timestamp":"2026-07-03T10:00:04.100Z","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}"#.to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        CodexStore::new(vec![root])
    }

    #[test]
    fn lists_native_sessions_from_one_message_copy() {
        let sessions = native_store("list").list().unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.id, ID);
        assert_eq!(session.title, "visible user");
        assert_eq!(session.cwd, "/tmp/native-codex");
        assert_eq!(session.message_count, 2);
        assert_eq!(
            session.updated_ms,
            parse_iso_utc_ms("2026-07-03T10:00:04.100Z").unwrap()
        );
    }

    #[test]
    fn reads_visible_messages_once_plus_tool_parts() {
        let session = native_store("read").read(ID).unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(parts_text(&session.messages[0].parts), "visible user");
        assert_eq!(parts_text(&session.messages[1].parts), "visible assistant");
        assert!(matches!(
            session.messages[2].parts[0],
            NativePart::ToolCall { .. }
        ));
        assert!(matches!(
            session.messages[3].parts[0],
            NativePart::ToolResult { .. }
        ));
        assert_eq!(session.metadata["codex_cli_version"], json!("0.143.0"));
        let texts: Vec<String> = turns(&session).into_iter().map(|turn| turn.text).collect();
        assert_eq!(texts, ["visible user", "visible assistant"]);
    }

    #[test]
    fn writes_rollout_and_index_that_read_back() {
        let root = temp_dir("codex-write");
        let store = CodexStore::new(vec![root.clone()]);
        let session = sample_session(Tool::Codex, ID);
        let written = store
            .write(&WriteRequest {
                session: &session,
                cli_version: "0.158.0",
                provenance: &json!({}),
                force: false,
                backup: false,
            })
            .unwrap();
        let rollout = fs::read_to_string(&written.reference).unwrap();
        assert!(rollout
            .lines()
            .next()
            .unwrap()
            .contains(r#""type":"session_meta""#));
        assert!(rollout.contains(r#""type":"user_message""#));
        let back = store.read(ID).unwrap();
        assert_eq!(back.title, session.title);
        assert_eq!(back.cwd, session.cwd);
        assert_eq!(turns(&back), turns(&session));
        let footprint = store.footprint(ID).unwrap();
        assert_eq!(footprint.paths, [PathBuf::from(&written.reference)]);
        assert_eq!(footprint.lines[0].lines.len(), 1);
    }

    #[test]
    fn note_appends_both_message_copies() {
        let store = native_store("note");
        let footprint = store
            .append_note(
                ID,
                &Note {
                    text: "moved",
                    cli_version: "0.158.0",
                },
            )
            .unwrap();
        assert_eq!(footprint.lines[0].lines.len(), 2);
        assert_eq!(store.transcript(ID).unwrap().last().unwrap().text, "moved");
    }
}
