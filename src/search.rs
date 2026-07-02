use std::collections::HashMap;

use crate::model::Session;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Every query token must be a case-insensitive substring of the
    /// name + full path + model blob. Substring (not subsequence) matching:
    /// over long path+title blobs, subsequence fuzzy matches nearly everything.
    Sessions,
    /// Sessions mode OR a case-insensitive substring match over message
    /// content — title/path hits are never dropped because the body differs.
    All,
}

impl SearchMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Sessions => Self::All,
            Self::All => Self::Sessions,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::All => "all",
        }
    }
}

/// Indices into `sessions` matching `query` under `mode`. `blobs` are the
/// precomputed lowercase search blobs (same order as `sessions`); `content`
/// maps session keys to lowercased transcript text as indexing completes.
pub fn filter_sessions(
    sessions: &[Session],
    blobs: &[String],
    query: &str,
    mode: SearchMode,
    content: &HashMap<String, String>,
) -> Vec<usize> {
    let tokens: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if tokens.is_empty() {
        return (0..sessions.len()).collect();
    }
    let needle = query.trim().to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
            let blob_hit = tokens.iter().all(|token| blobs[index].contains(token));
            let hit = match mode {
                SearchMode::Sessions => blob_hit,
                SearchMode::All => {
                    blob_hit
                        || content
                            .get(&session.key())
                            .is_some_and(|text| text.contains(&needle))
                }
            };
            hit.then_some(index)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tool;

    fn session(id: &str, title: &str, cwd: &str) -> Session {
        Session {
            tool: Tool::Claude,
            id: id.to_string(),
            title: title.to_string(),
            cwd: cwd.to_string(),
            created_ms: 0,
            updated_ms: 0,
            message_count: 0,
            model: None,
            source_ref: String::new(),
            extras: Vec::new(),
        }
    }

    fn blobs(sessions: &[Session]) -> Vec<String> {
        sessions.iter().map(Session::search_blob).collect()
    }

    #[test]
    fn sessions_mode_matches_path_and_title() {
        let sessions = vec![
            session("a", "Fix rate limiter", "/Users/me/dotfiles"),
            session("b", "Other work", "/Users/me/project"),
        ];
        let blobs = blobs(&sessions);
        let content = HashMap::new();
        let hits = filter_sessions(
            &sessions,
            &blobs,
            "dotfiles",
            SearchMode::Sessions,
            &content,
        );
        assert_eq!(hits, vec![0]);
        let hits = filter_sessions(
            &sessions,
            &blobs,
            "rate lim",
            SearchMode::Sessions,
            &content,
        );
        assert_eq!(hits, vec![0]);
        let hits = filter_sessions(&sessions, &blobs, "", SearchMode::Sessions, &content);
        assert_eq!(hits, vec![0, 1]);
    }

    #[test]
    fn all_mode_unions_content_and_blob_hits() {
        let sessions = vec![
            session("a", "Fix rate limiter", "/Users/me/dotfiles"),
            session("b", "Other work", "/Users/me/project"),
        ];
        let blobs = blobs(&sessions);
        let mut content = HashMap::new();
        content.insert(
            "claude:b".to_string(),
            "we discussed the rate limiter here".to_string(),
        );
        // content-only hit (b) unions with title hit (a)
        let hits = filter_sessions(&sessions, &blobs, "rate limiter", SearchMode::All, &content);
        assert_eq!(hits, vec![0, 1]);
        // path hit still matches in All mode even with no content indexed
        let hits = filter_sessions(&sessions, &blobs, "dotfiles", SearchMode::All, &content);
        assert_eq!(hits, vec![0]);
    }
}
