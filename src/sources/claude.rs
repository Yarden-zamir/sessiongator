use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::model::{parse_iso_utc_ms, Session, Tool};
use crate::sources::{SessionSource, Turn};

/// Reads Claude Code JSONL session logs from `<config root>/projects/`.
///
/// The project folder name is a lossy encoding of the working directory
/// (`/` replaced by `-`), so the real `cwd` is always read from event content,
/// never decoded from the path.
pub struct ClaudeSource {
    projects_dir: PathBuf,
}

impl ClaudeSource {
    pub fn from_env() -> Self {
        let root = std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_default();
                Path::new(&home).join(".claude")
            });
        Self::new(root)
    }

    pub fn new(root: PathBuf) -> Self {
        Self {
            projects_dir: root.join("projects"),
        }
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let Ok(projects) = fs::read_dir(&self.projects_dir) else {
            return files;
        };
        for project in projects.flatten() {
            let path = project.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(entries) = fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jsonl") {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    fn find_file(&self, id: &str) -> Option<PathBuf> {
        self.session_files()
            .into_iter()
            .find(|path| path.file_stem().is_some_and(|stem| stem == id))
    }
}

impl SessionSource for ClaudeSource {
    fn tool(&self) -> Tool {
        Tool::Claude
    }

    fn available(&self) -> bool {
        self.projects_dir.is_dir()
    }

    fn list(&self) -> Result<Vec<Session>, String> {
        Ok(self
            .session_files()
            .into_iter()
            .filter_map(|path| parse_session_meta(&path))
            .collect())
    }

    fn transcript(&self, id: &str) -> Result<Vec<Turn>, String> {
        let path = self
            .find_file(id)
            .ok_or_else(|| format!("claude session {id} not found"))?;
        let file = fs::File::open(&path).map_err(|error| error.to_string())?;
        let mut turns = Vec::new();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let role = event.get("type").and_then(Value::as_str).unwrap_or("");
            if role != "user" && role != "assistant" {
                continue;
            }
            for text in event_texts(event.get("message")) {
                turns.push(Turn {
                    role: role.to_string(),
                    text,
                });
            }
        }
        Ok(turns)
    }
}

/// Scan one JSONL file for session metadata. Corrupt lines are skipped; a
/// session is never dropped for one bad event.
fn parse_session_meta(path: &Path) -> Option<Session> {
    let id = path.file_stem()?.to_str()?.to_string();
    let file = fs::File::open(path).ok()?;

    let mut cwd = None;
    let mut branch: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut model: Option<String> = None;
    let mut created_ms = None;
    let mut updated_ms = None;
    let mut count: u32 = 0;

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = non_empty_str(event.get("cwd"));
        }
        if branch.is_none() {
            branch = non_empty_str(event.get("gitBranch"));
        }
        if let Some(ts) = event
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_utc_ms)
        {
            if created_ms.is_none() {
                created_ms = Some(ts);
            }
            updated_ms = Some(ts);
        }
        match event.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                ai_title = non_empty_str(event.get("aiTitle")).or(ai_title);
            }
            Some("user") => {
                count += 1;
                if first_user.is_none() {
                    first_user = first_user_text(event.get("message"));
                }
            }
            Some("assistant") => {
                count += 1;
                if let Some(value) = event
                    .get("message")
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str)
                {
                    model = Some(value.to_string());
                }
            }
            _ => {}
        }
    }

    let title = ai_title
        .or_else(|| first_user.map(|text| text.chars().take(120).collect()))
        .map(|text: String| crate::model::clean_title(&text))
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

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Real typed prompt text from a user event (skips tool_result payloads).
fn first_user_text(message: Option<&Value>) -> Option<String> {
    let message = message?;
    if let Some(text) = message.as_str() {
        return (!text.is_empty()).then(|| text.to_string());
    }
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return (!text.is_empty()).then(|| text.to_string());
    }
    content.as_array()?.iter().find_map(|block| {
        (block.get("type").and_then(Value::as_str) == Some("text"))
            .then(|| non_empty_str(block.get("text")))
            .flatten()
    })
}

/// Visible text of one event: plain strings and `text` content blocks.
/// `thinking`, `tool_use`, and `tool_result` blocks are excluded.
fn event_texts(message: Option<&Value>) -> Vec<String> {
    let Some(message) = message else {
        return Vec::new();
    };
    if let Some(text) = message.as_str() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }
    let Some(content) = message.get("content") else {
        return Vec::new();
    };
    if let Some(text) = content.as_str() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| non_empty_str(block.get("text")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("sessiongator-claude-{name}-{}", std::process::id()));
        let project = root.join("projects").join("-Users-me-Github-demo");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&project).unwrap();
        let sid = "11111111-2222-3333-4444-555555555555";
        let events = [
            r#"{"type":"user","cwd":"/Users/me/Github/demo","gitBranch":"main","timestamp":"2026-06-29T16:11:35.000Z","sessionId":"S","message":{"role":"user","content":"please fix the rate limiter bug"}}"#,
            r#"{"type":"assistant","cwd":"/Users/me/Github/demo","timestamp":"2026-06-29T16:12:00.000Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"thinking","thinking":"secret reasoning"},{"type":"text","text":"Fixed the rate limiter."}]}}"#,
            "this line is corrupt json",
            r#"{"type":"user","timestamp":"2026-06-29T16:12:30.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"TOOLNOISE"}]}}"#,
            r#"{"type":"ai-title","aiTitle":"Fix the rate limiter","sessionId":"S"}"#,
        ];
        fs::write(project.join(format!("{sid}.jsonl")), events.join("\n")).unwrap();
        root
    }

    #[test]
    fn lists_metadata_from_content_not_path() {
        let root = fixture_root("list");
        let source = ClaudeSource::new(root);
        let sessions = source.list().unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.title, "Fix the rate limiter"); // aiTitle wins
        assert_eq!(session.cwd, "/Users/me/Github/demo"); // from content
        assert_eq!(session.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(session.message_count, 3);
        assert_eq!(
            session.extras,
            vec![("branch".to_string(), "main".to_string())]
        );
        assert!(session.created_ms > 0 && session.updated_ms >= session.created_ms);
    }

    #[test]
    fn transcript_excludes_thinking_and_tool_results() {
        let root = fixture_root("transcript");
        let source = ClaudeSource::new(root);
        let id = source.list().unwrap()[0].id.clone();
        let turns = source.transcript(&id).unwrap();
        let texts: Vec<&str> = turns.iter().map(|turn| turn.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["please fix the rate limiter bug", "Fixed the rate limiter."]
        );
        assert!(!texts.iter().any(|text| text.contains("secret reasoning")));
        assert!(!texts.iter().any(|text| text.contains("TOOLNOISE")));
    }

    #[test]
    fn title_falls_back_to_first_user_text() {
        let value: Value = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"hello world"}]}"#,
        )
        .unwrap();
        assert_eq!(
            first_user_text(Some(&value)).as_deref(),
            Some("hello world")
        );
        let tool: Value = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"tool_result","content":"X"}]}"#,
        )
        .unwrap();
        assert_eq!(first_user_text(Some(&tool)), None);
    }
}
