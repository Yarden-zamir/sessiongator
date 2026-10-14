use std::cmp::Reverse;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, clap::ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Opencode,
    Codex,
    Copilot,
}

impl Tool {
    pub const ALL: [Tool; 4] = [Tool::Claude, Tool::Opencode, Tool::Codex, Tool::Copilot];

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Claude => "◆",
            Self::Opencode => "◇",
            Self::Codex => "●",
            Self::Copilot => "○",
        }
    }

    /// Command that resumes session `id`. `scripts/sessiongator.zsh` builds the
    /// same commands; update both together.
    pub fn resume_command(self, id: &str) -> Vec<String> {
        let id = id.to_string();
        match self {
            Self::Claude => vec!["claude".into(), "--resume".into(), id],
            Self::Opencode => vec!["opencode".into(), "--session".into(), id],
            Self::Codex => vec!["codex".into(), "resume".into(), id],
            Self::Copilot => vec!["copilot".into(), format!("--resume={id}")],
        }
    }

    pub fn default_convert_target(self) -> Self {
        match self {
            Self::Claude => Self::Opencode,
            Self::Opencode | Self::Codex | Self::Copilot => Self::Claude,
        }
    }
}

/// One AI coding session, normalized across sources. Carries only fields every
/// source can provide; anything tool-specific goes into `extras` as
/// display-ready key/value pairs.
#[derive(Clone, Debug)]
pub struct Session {
    pub tool: Tool,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub message_count: u32,
    pub model: Option<String>,
    pub source_ref: String,
    pub extras: Vec<(String, String)>,
}

impl Session {
    /// Stable cross-source key ("<tool>:<native id>").
    pub fn key(&self) -> String {
        format!("{}:{}", self.tool.name(), self.id)
    }

    /// Lowercased blob used for name/path/model matching.
    pub fn search_blob(&self) -> String {
        format!(
            "{} {} {} {}",
            self.cwd,
            shorten_home(&self.cwd),
            self.title,
            self.model.as_deref().unwrap_or("")
        )
        .to_lowercase()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SortMode {
    Updated,
    Created,
    #[value(name = "msgs")]
    Messages,
    Path,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Updated => Self::Created,
            Self::Created => Self::Messages,
            Self::Messages => Self::Path,
            Self::Path => Self::Updated,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Created => "created",
            Self::Messages => "msgs",
            Self::Path => "path",
        }
    }
}

pub fn sort_sessions(sessions: &mut [Session], mode: SortMode) {
    match mode {
        SortMode::Updated => sessions.sort_by_key(|session| Reverse(session.updated_ms)),
        SortMode::Created => sessions.sort_by_key(|session| Reverse(session.created_ms)),
        SortMode::Messages => sessions.sort_by_key(|session| Reverse(session.message_count)),
        SortMode::Path => sessions.sort_by(|a, b| {
            (a.cwd.to_lowercase(), a.updated_ms).cmp(&(b.cwd.to_lowercase(), b.updated_ms))
        }),
    }
}

/// Collapse all whitespace runs (incl. newlines/tabs) to single spaces so
/// titles are always safe for one-line display and tab-separated output.
pub fn clean_title(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A shell line that resumes `id` with `tool`, after a `cd` into `cwd` when
/// one is given.
pub fn resume_line(tool: Tool, id: &str, cwd: Option<&str>) -> String {
    let command: Vec<String> = tool
        .resume_command(id)
        .iter()
        .map(|arg| shell_quote(arg))
        .collect();
    match cwd.filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), command.join(" ")),
        None => command.join(" "),
    }
}

/// `value` as one POSIX shell word.
pub fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./=:@%+".contains(ch));
    if plain {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn shorten_home(path: &str) -> String {
    gator::text::collapse_home_env(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_unsafe_shell_words() {
        assert_eq!(shell_quote("ses_01K-abc"), "ses_01K-abc");
        assert_eq!(shell_quote("--resume=ab12"), "--resume=ab12");
        assert_eq!(shell_quote("/tmp/my dir"), "'/tmp/my dir'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(
            resume_line(Tool::Copilot, "ab", Some("/tmp/a b")),
            "cd '/tmp/a b' && copilot --resume=ab"
        );
        assert_eq!(resume_line(Tool::Codex, "ab", None), "codex resume ab");
    }

    #[test]
    fn sorting() {
        let mk = |id: &str, updated: i64, created: i64, msgs: u32, cwd: &str| Session {
            tool: Tool::Claude,
            id: id.to_string(),
            title: String::new(),
            cwd: cwd.to_string(),
            created_ms: created,
            updated_ms: updated,
            message_count: msgs,
            model: None,
            source_ref: String::new(),
            extras: Vec::new(),
        };
        let mut sessions = vec![mk("a", 10, 1, 5, "/z"), mk("b", 1, 10, 50, "/a")];
        sort_sessions(&mut sessions, SortMode::Updated);
        assert_eq!(sessions[0].id, "a");
        sort_sessions(&mut sessions, SortMode::Created);
        assert_eq!(sessions[0].id, "b");
        sort_sessions(&mut sessions, SortMode::Messages);
        assert_eq!(sessions[0].id, "b");
        sort_sessions(&mut sessions, SortMode::Path);
        assert_eq!(sessions[0].id, "b");
    }
}
