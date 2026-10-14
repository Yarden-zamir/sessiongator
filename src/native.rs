//! The tool-neutral session model that conversion reads into and writes
//! from. It keeps every part a reader understands, so a writer can map as
//! much as its target format can hold.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{json, Value};

use crate::model::Tool;
use crate::time::now_ms;

#[derive(Clone, Debug)]
pub struct NativeSession {
    pub tool: Tool,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub model: Option<ModelRef>,
    pub messages: Vec<NativeMessage>,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
pub struct ModelRef {
    pub provider_id: Option<String>,
    pub id: String,
}

#[derive(Clone, Debug)]
pub struct NativeMessage {
    pub role: NativeRole,
    pub created_ms: i64,
    pub updated_ms: Option<i64>,
    pub parts: Vec<NativePart>,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeRole {
    System,
    User,
    Assistant,
    Shell,
    Compaction,
    Unknown(String),
}

impl NativeRole {
    pub fn from_name(value: &str) -> Self {
        match value {
            "system" => Self::System,
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "shell" => Self::Shell,
            "compaction" => Self::Compaction,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Shell => "shell",
            Self::Compaction => "compaction",
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Clone, Debug)]
pub enum NativePart {
    Text(String),
    Reasoning {
        text: String,
        metadata: Option<Value>,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        content: Value,
        is_error: bool,
    },
    File {
        name: Option<String>,
        mime: Option<String>,
        url: Option<String>,
        path: Option<String>,
    },
    /// A part the reader does not understand, kept so the plan can report it
    /// as dropped.
    Raw {
        kind: String,
        value: Value,
    },
}

/// One transcript line for display and content search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub role: String,
    pub text: String,
}

impl NativeMessage {
    pub fn text(role: NativeRole, created_ms: i64, text: String) -> Self {
        Self {
            role,
            created_ms,
            updated_ms: None,
            parts: vec![NativePart::Text(text)],
            metadata: BTreeMap::new(),
        }
    }
}

/// Transcript text: the text parts of user and assistant messages. Reasoning,
/// tool calls, tool output, and control messages are not transcript text.
pub fn turns(session: &NativeSession) -> Vec<Turn> {
    session
        .messages
        .iter()
        .filter(|message| matches!(message.role, NativeRole::User | NativeRole::Assistant))
        .flat_map(|message| {
            message.parts.iter().filter_map(|part| match part {
                NativePart::Text(text) if !text.trim().is_empty() => Some(Turn {
                    role: message.role.name().to_string(),
                    text: text.clone(),
                }),
                _ => None,
            })
        })
        .collect()
}

pub fn parts_to_json(parts: &[NativePart]) -> Value {
    Value::Array(parts.iter().map(part_to_json).collect())
}

fn part_to_json(part: &NativePart) -> Value {
    match part {
        NativePart::Text(text) => json!({ "type": "text", "text": text }),
        NativePart::Reasoning { text, metadata } => {
            json!({ "type": "reasoning", "text": text, "metadata": metadata })
        }
        NativePart::ToolCall { id, name, input } => {
            json!({ "type": "tool_call", "id": id, "name": name, "input": input })
        }
        NativePart::ToolResult {
            id,
            content,
            is_error,
        } => json!({ "type": "tool_result", "id": id, "content": content, "is_error": is_error }),
        NativePart::File {
            name,
            mime,
            url,
            path,
        } => json!({ "type": "file", "name": name, "mime": mime, "url": url, "path": path }),
        NativePart::Raw { kind, value } => json!({ "type": "raw", "kind": kind, "value": value }),
    }
}

/// Inverse of `parts_to_json`; writers store this in a `sessiongatorParts`
/// field so a read back is lossless.
pub fn parts_from_json(value: &Value) -> Option<Vec<NativePart>> {
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(part_from_json)
            .collect(),
    )
}

fn part_from_json(value: &Value) -> Option<NativePart> {
    match value.get("type")?.as_str()? {
        "text" => non_empty_string(value.get("text")).map(NativePart::Text),
        "reasoning" => non_empty_string(value.get("text")).map(|text| NativePart::Reasoning {
            text,
            metadata: value
                .get("metadata")
                .cloned()
                .filter(|value| !value.is_null()),
        }),
        "tool_call" => Some(NativePart::ToolCall {
            id: non_empty_string(value.get("id"))?,
            name: non_empty_string(value.get("name"))?,
            input: value.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => Some(NativePart::ToolResult {
            id: non_empty_string(value.get("id"))?,
            content: value.get("content").cloned().unwrap_or(Value::Null),
            is_error: value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "file" => Some(NativePart::File {
            name: non_empty_string(value.get("name")),
            mime: non_empty_string(value.get("mime")),
            url: non_empty_string(value.get("url")),
            path: non_empty_string(value.get("path")),
        }),
        "raw" => Some(NativePart::Raw {
            kind: non_empty_string(value.get("kind"))?,
            value: value.get("value").cloned().unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

pub fn first_text_part(parts: &[NativePart]) -> Option<String> {
    parts.iter().find_map(|part| match part {
        NativePart::Text(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    })
}

pub fn parts_text(parts: &[NativePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            NativePart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A title from the first user text, as the tools show untitled sessions.
pub fn title_from_text(text: &str) -> String {
    crate::model::clean_title(&text.chars().take(120).collect::<String>())
}

pub fn content_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(content_to_string)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string()),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

pub fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

pub fn model_json(model: &ModelRef) -> Value {
    json!({ "id": model.id, "providerID": model.provider_id.as_deref().unwrap_or("imported") })
}

pub fn model_ref_from_json(value: &Value) -> Option<ModelRef> {
    Some(ModelRef {
        provider_id: value
            .get("providerID")
            .or_else(|| value.get("provider_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        id: value
            .get("id")
            .or_else(|| value.get("modelID"))
            .and_then(Value::as_str)?
            .to_string(),
    })
}

pub fn part_counts(messages: &[NativeMessage]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for part in messages.iter().flat_map(|message| &message.parts) {
        let key = match part {
            NativePart::Text(_) => "text",
            NativePart::Reasoning { .. } => "reasoning",
            NativePart::ToolCall { .. } => "tool_call",
            NativePart::ToolResult { .. } => "tool_result",
            NativePart::File { .. } => "file",
            NativePart::Raw { .. } => "raw",
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique id in the `<prefix>_...` shape opencode uses.
pub fn generated_id(prefix: &str) -> String {
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}_sessiongator_{:x}_{:x}_{seq:x}",
        now_ms(),
        std::process::id()
    )
}

/// A unique version-4-shaped UUID. Not random: time, process id, and a
/// counter make it unique on one machine, which is all a new session needs.
pub fn generated_uuid() -> String {
    let now = now_ms() as u128;
    let pid = u128::from(std::process::id());
    let seq = u128::from(ID_COUNTER.fetch_add(1, Ordering::Relaxed));
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (now >> 16) as u32,
        (now & 0xffff) as u16,
        (pid & 0xfff) as u16,
        ((now >> 4) & 0xfff) as u16,
        (now ^ (pid << 32) ^ seq) & 0xffff_ffff_ffff
    )
}

/// FNV-1a hash, hex encoded. Used for schema fingerprints only.
pub fn short_hash(value: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_turns_are_user_and_assistant_text_only() {
        let message = |role, parts| NativeMessage {
            role,
            created_ms: 0,
            updated_ms: None,
            parts,
            metadata: BTreeMap::new(),
        };
        let session = NativeSession {
            tool: Tool::Claude,
            id: "s".to_string(),
            title: String::new(),
            cwd: String::new(),
            created_ms: 0,
            updated_ms: 0,
            model: None,
            messages: vec![
                message(NativeRole::User, vec![NativePart::Text("ask".to_string())]),
                message(
                    NativeRole::Assistant,
                    vec![
                        NativePart::Reasoning {
                            text: "hidden".to_string(),
                            metadata: None,
                        },
                        NativePart::ToolCall {
                            id: "t".to_string(),
                            name: "Read".to_string(),
                            input: Value::Null,
                        },
                        NativePart::Text("answer".to_string()),
                    ],
                ),
                message(
                    NativeRole::System,
                    vec![NativePart::Text("sys".to_string())],
                ),
            ],
            metadata: BTreeMap::new(),
        };
        let texts: Vec<(String, String)> = turns(&session)
            .into_iter()
            .map(|turn| (turn.role, turn.text))
            .collect();
        assert_eq!(
            texts,
            [
                ("user".to_string(), "ask".to_string()),
                ("assistant".to_string(), "answer".to_string())
            ]
        );
    }

    #[test]
    fn parts_json_roundtrip_keeps_every_kind() {
        let parts = vec![
            NativePart::Text("t".to_string()),
            NativePart::Reasoning {
                text: "r".to_string(),
                metadata: Some(json!({ "signature": "s" })),
            },
            NativePart::ToolCall {
                id: "c".to_string(),
                name: "n".to_string(),
                input: json!({ "a": 1 }),
            },
            NativePart::ToolResult {
                id: "c".to_string(),
                content: json!("ok"),
                is_error: true,
            },
            NativePart::File {
                name: Some("f".to_string()),
                mime: None,
                url: None,
                path: Some("/p".to_string()),
            },
            NativePart::Raw {
                kind: "k".to_string(),
                value: json!(1),
            },
        ];
        let back = parts_from_json(&parts_to_json(&parts)).unwrap();
        assert_eq!(parts_to_json(&back), parts_to_json(&parts));
    }
}
