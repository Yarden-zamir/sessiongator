use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::model::{clean_title, parse_iso_utc_ms, Session, Tool};
use crate::sources::{SessionSource, Turn};

pub struct CodexSource {
    roots: Vec<PathBuf>,
}

impl CodexSource {
    pub fn from_env() -> Self {
        Self {
            roots: codex_roots_from_env(),
        }
    }

    #[cfg(test)]
    pub fn new(root: PathBuf) -> Self {
        Self { roots: vec![root] }
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

    fn find_file(&self, id: &str) -> Option<PathBuf> {
        self.session_files()
            .into_iter()
            .find(|path| codex_file_matches_id(path, id))
    }
}

fn codex_roots_from_env() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(root) = std::env::var("CODEX_HOME") {
        roots.push(PathBuf::from(root));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    roots.push(Path::new(&home).join(".codex"));
    roots.sort();
    roots.dedup();
    roots
}

impl SessionSource for CodexSource {
    fn tool(&self) -> Tool {
        Tool::Codex
    }

    fn available(&self) -> bool {
        self.roots
            .iter()
            .any(|root| root.join("sessions").is_dir() || root.join("archived_sessions").is_dir())
    }

    fn list(&self) -> Result<Vec<Session>, String> {
        Ok(self
            .session_files()
            .into_iter()
            .filter_map(|path| parse_codex_meta(&path))
            .collect())
    }

    fn transcript(&self, id: &str) -> Result<Vec<Turn>, String> {
        let path = self
            .find_file(id)
            .ok_or_else(|| format!("codex session {id} not found"))?;
        let file = fs::File::open(path).map_err(|error| error.to_string())?;
        let mut response_turns = Vec::new();
        let mut event_turns = Vec::new();
        let mut auxiliary_turns = Vec::new();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some((role, texts, source)) = message_role_and_texts(&event) {
                for text in texts {
                    let turn = Turn {
                        role: role.clone(),
                        text,
                    };
                    match source {
                        CodexMessageSource::ResponseItem => response_turns.push(turn),
                        CodexMessageSource::ResponseAuxiliary => auxiliary_turns.push(turn),
                        CodexMessageSource::EventMsg => event_turns.push(turn),
                        CodexMessageSource::Sessiongator => response_turns.push(turn),
                    }
                }
            }
        }
        let mut turns = if event_turns.is_empty() {
            response_turns
        } else {
            event_turns
        };
        turns.extend(auxiliary_turns);
        Ok(turns)
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

fn codex_file_matches_id(path: &Path, id: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains(id))
}

fn codex_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("rollout-")?;
    if rest.len() > 20 {
        return Some(rest[20..].to_string());
    }
    Some(rest.to_string())
}

fn parse_codex_meta(path: &Path) -> Option<Session> {
    let id = codex_id_from_path(path)?;
    let file = fs::File::open(path).ok()?;
    let mut title = None;
    let mut cwd = None;
    let mut model = None;
    let mut created_ms = None;
    let mut updated_ms = None;
    let mut response_first_user = None;
    let mut event_first_user = None;
    let mut response_count = 0u32;
    let mut event_count = 0u32;

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(meta) = codex_meta(&event) {
            title = non_empty_str(meta.get("title")).or(title);
            cwd = non_empty_str(meta.get("cwd")).or(cwd);
            if let Some(provider) = non_empty_str(meta.get("model_provider")) {
                model = Some(provider);
            }
            if let Some(ts) =
                non_empty_str(meta.get("timestamp")).and_then(|value| parse_iso_utc_ms(&value))
            {
                created_ms = Some(created_ms.unwrap_or(ts));
                updated_ms = Some(updated_ms.unwrap_or(ts));
            }
            continue;
        }
        if let Some((role, texts, source)) = message_role_and_texts(&event) {
            match source {
                CodexMessageSource::ResponseItem | CodexMessageSource::Sessiongator => {
                    response_count += 1;
                }
                CodexMessageSource::ResponseAuxiliary => {}
                CodexMessageSource::EventMsg => event_count += 1,
            }
            let timestamp = event
                .get("created_ms")
                .or_else(|| event.get("timestamp"))
                .and_then(Value::as_i64)
                .unwrap_or_else(|| updated_ms.unwrap_or(0));
            if created_ms.is_none() && timestamp > 0 {
                created_ms = Some(timestamp);
            }
            if timestamp > 0 {
                updated_ms = Some(timestamp);
            }
            if role == "user" {
                match source {
                    CodexMessageSource::ResponseItem | CodexMessageSource::Sessiongator => {
                        if response_first_user.is_none() {
                            response_first_user = texts.into_iter().next();
                        }
                    }
                    CodexMessageSource::ResponseAuxiliary => {}
                    CodexMessageSource::EventMsg => {
                        if event_first_user.is_none() {
                            event_first_user = texts.into_iter().next();
                        }
                    }
                }
            }
            if model.is_none() {
                model = event
                    .get("model")
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
    }

    let updated = updated_ms.or(created_ms).unwrap_or(0);
    Some(Session {
        tool: Tool::Codex,
        id,
        title: title
            .or_else(|| {
                event_first_user
                    .or(response_first_user)
                    .map(|text| text.chars().take(120).collect())
            })
            .map(|text: String| clean_title(&text))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "(no title)".to_string()),
        cwd: cwd.unwrap_or_default(),
        created_ms: created_ms.unwrap_or(updated),
        updated_ms: updated,
        message_count: if event_count == 0 {
            response_count
        } else {
            event_count
        },
        model,
        source_ref: path.display().to_string(),
        extras: Vec::new(),
    })
}

fn codex_meta(event: &Value) -> Option<&Value> {
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

#[derive(Clone, Copy)]
enum CodexMessageSource {
    ResponseItem,
    ResponseAuxiliary,
    EventMsg,
    Sessiongator,
}

fn message_role_and_texts(event: &Value) -> Option<(String, Vec<String>, CodexMessageSource)> {
    if event.get("type").and_then(Value::as_str) == Some("response_item") {
        let payload = event.get("payload")?;
        return codex_response_item_turn(payload);
    }

    if event.get("type").and_then(Value::as_str) == Some("event_msg") {
        let payload = event.get("payload")?;
        let (role, text) = match payload.get("type").and_then(Value::as_str) {
            Some("user_message") => ("user", non_empty_str(payload.get("message"))),
            Some("agent_message") => ("assistant", non_empty_str(payload.get("message"))),
            _ => return None,
        };
        return text.map(|text| (role.to_string(), vec![text], CodexMessageSource::EventMsg));
    }

    let item = event.get("item").unwrap_or(event);
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let role = non_empty_str(item.get("role"))?;
    let texts = item
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| non_empty_str(part.get("text")))
        .collect::<Vec<_>>();
    (!texts.is_empty()).then_some((role, texts, CodexMessageSource::Sessiongator))
}

fn codex_response_item_turn(payload: &Value) -> Option<(String, Vec<String>, CodexMessageSource)> {
    match payload.get("type").and_then(Value::as_str)? {
        "message" => {
            let role = non_empty_str(payload.get("role"))?;
            if role != "user" && role != "assistant" {
                return None;
            }
            let texts = codex_content_texts(payload.get("content"));
            (!texts.is_empty()).then_some((role, texts, CodexMessageSource::ResponseItem))
        }
        "function_call" => {
            let name = non_empty_str(payload.get("name")).unwrap_or_else(|| "unknown".to_string());
            let args = payload
                .get("arguments")
                .map(Value::to_string)
                .unwrap_or_else(|| "{}".to_string());
            Some((
                "tool".to_string(),
                vec![format!("call {name} {args}")],
                CodexMessageSource::ResponseAuxiliary,
            ))
        }
        "function_call_output" => non_empty_str(payload.get("output")).map(|text| {
            (
                "tool".to_string(),
                vec![format!("result {text}")],
                CodexMessageSource::ResponseAuxiliary,
            )
        }),
        "web_search_call" => Some((
            "tool".to_string(),
            vec!["web_search".to_string()],
            CodexMessageSource::ResponseAuxiliary,
        )),
        _ => None,
    }
}

fn codex_content_texts(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => non_empty_str(part.get("text")),
            _ => None,
        })
        .collect()
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sessiongator-codex-source-{name}-{}",
            std::process::id()
        ));
        let dir = root.join("sessions/2026/07/03");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("rollout-2026-07-03T10-00-01-11111111-2222-4333-8444-555555555555.jsonl"),
            [
                r#"{"item":{"type":"session_meta","meta":{"id":"11111111-2222-4333-8444-555555555555","title":"Codex demo","cwd":"/tmp/codex-demo","timestamp":"2026-07-03T10:00:01.000Z","model_provider":"openai"}}}"#,
                r#"{"type":"message","role":"user","created_ms":1783072801000,"parts":[{"type":"text","text":"hello codex"}]}"#,
                r#"{"type":"message","role":"assistant","created_ms":1783072802000,"parts":[{"type":"reasoning","text":"hidden"},{"type":"text","text":"hi back"}]}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        root
    }

    #[test]
    fn lists_and_reads_codex_sessions() {
        let root = fixture_root("list");
        let source = CodexSource::new(root);
        let sessions = source.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].tool, Tool::Codex);
        assert_eq!(sessions[0].title, "Codex demo");
        assert_eq!(sessions[0].cwd, "/tmp/codex-demo");
        assert_eq!(sessions[0].message_count, 2);
        let turns = source.transcript(&sessions[0].id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "hello codex");
        assert_eq!(turns[1].text, "hi back");
        assert!(!turns.iter().any(|turn| turn.text.contains("hidden")));
    }

    #[test]
    fn reads_native_codex_response_items_without_event_msg_duplicates() {
        let root = std::env::temp_dir().join(format!(
            "sessiongator-codex-native-source-{}",
            std::process::id()
        ));
        let dir = root.join("sessions/2026/07/03");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&dir).unwrap();
        let id = "22222222-3333-4444-8555-666666666666";
        fs::write(
            dir.join(format!("rollout-2026-07-03T10-00-01-{id}.jsonl")),
            [
                format!(r#"{{"type":"session_meta","timestamp":"2026-07-03T10:00:01.000Z","payload":{{"id":"{id}","cwd":"/tmp/native-codex","timestamp":"2026-07-03T10:00:01.000Z","cli_version":"0.143.0","model_provider":"openai"}}}}"#),
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
        let source = CodexSource::new(root);
        let sessions = source.list().unwrap();
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[0].title, "visible user");
        let turns = source.transcript(id).unwrap();
        assert_eq!(turns.len(), 4);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "visible user");
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].text, "visible assistant");
        assert_eq!(turns[2].role, "tool");
        assert!(turns[2].text.contains("call shell"));
        assert_eq!(turns[3].role, "tool");
        assert!(turns[3].text.contains("result ok"));
    }
}
