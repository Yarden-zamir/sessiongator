use std::collections::HashMap;

use crate::model::{clean_title, Session};
use crate::time::{now_ms, rel_time};

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

/// The one session that `reference` names. An exact `tool:id` or id wins.
/// Otherwise the candidates are id prefixes plus title/path/model matches, and
/// the reference must match exactly one of them, so a word that happens to be
/// a hex id prefix never silently picks another session.
pub fn resolve_session<'a>(
    sessions: &'a [Session],
    reference: &str,
) -> Result<&'a Session, String> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err("empty session reference".to_string());
    }
    let exact: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id == reference || session.key() == reference)
        .collect();
    if let [session] = exact.as_slice() {
        return Ok(session);
    }
    if !exact.is_empty() {
        return Err(ambiguous(reference, exact));
    }
    let blobs: Vec<String> = sessions.iter().map(Session::search_blob).collect();
    let text_hits = filter_sessions(
        sessions,
        &blobs,
        reference,
        SearchMode::Sessions,
        &HashMap::new(),
    );
    let candidates: Vec<&Session> = sessions
        .iter()
        .enumerate()
        .filter(|(index, session)| {
            session.id.starts_with(reference)
                || session.key().starts_with(reference)
                || text_hits.contains(index)
        })
        .map(|(_, session)| session)
        .collect();
    match candidates.as_slice() {
        [] => Err(format!("no session matches {reference:?}")),
        [session] => Ok(session),
        _ => Err(ambiguous(reference, candidates)),
    }
}

fn ambiguous(reference: &str, mut candidates: Vec<&Session>) -> String {
    const SHOWN: usize = 10;
    candidates.sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
    let now = now_ms();
    let mut message = format!(
        "{reference:?} matches {} sessions; pass a longer id or tool:id:",
        candidates.len()
    );
    for session in candidates.iter().take(SHOWN) {
        message.push_str(&format!(
            "\n  {}  {}  {}",
            session.key(),
            rel_time(session.updated_ms, now),
            session.title
        ));
    }
    if candidates.len() > SHOWN {
        message.push_str(&format!("\n  … {} more", candidates.len() - SHOWN));
    }
    message
}

/// One-line excerpt of `text` around the first case-insensitive `needle`.
pub fn match_snippet(text: &str, needle: &str, context_chars: usize) -> Option<String> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let byte = lower.find(&needle)?;
    // Lowercasing can change the char count of a few scripts; an offset of a
    // char or two only shifts the excerpt window.
    let start_char = lower[..byte].chars().count();
    let needle_chars = needle.chars().count();
    let from = start_char.saturating_sub(context_chars);
    let excerpt: String = text
        .chars()
        .skip(from)
        .take(needle_chars + 2 * context_chars)
        .collect();
    let excerpt = clean_title(&excerpt);
    let prefix = if from > 0 { "…" } else { "" };
    Some(format!("{prefix}{excerpt}…"))
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
    fn resolves_exact_ids_prefixes_and_unique_text() {
        let mut sessions = vec![
            session("aaaa1111", "Fix rate limiter", "/Users/me/dotfiles"),
            session("aaaa2222", "Other work", "/Users/me/project"),
        ];
        sessions[1].tool = Tool::Codex;
        assert_eq!(
            resolve_session(&sessions, "aaaa1111").unwrap().id,
            "aaaa1111"
        );
        assert_eq!(
            resolve_session(&sessions, "codex:aaaa2222").unwrap().id,
            "aaaa2222"
        );
        assert_eq!(resolve_session(&sessions, "aaaa2").unwrap().id, "aaaa2222");
        assert_eq!(
            resolve_session(&sessions, "rate lim").unwrap().id,
            "aaaa1111"
        );
        assert!(resolve_session(&sessions, "aaaa").is_err());
        assert!(resolve_session(&sessions, "/Users/me").is_err());
        assert!(resolve_session(&sessions, "nothing here").is_err());
        assert!(resolve_session(&sessions, " ").is_err());
    }

    #[test]
    fn id_prefix_that_also_matches_other_text_is_ambiguous() {
        let sessions = vec![
            session("add12345", "Unrelated", "/tmp/a"),
            session("ffff0000", "add a button", "/tmp/b"),
        ];
        let error = resolve_session(&sessions, "add").unwrap_err();
        assert!(error.contains("matches 2 sessions"), "{error}");
    }

    #[test]
    fn snippet_surrounds_the_first_case_insensitive_match() {
        let snippet = match_snippet("one two\nTHREE four five", "three", 4).unwrap();
        assert!(snippet.contains("THREE"), "{snippet}");
        assert!(snippet.starts_with('…'), "{snippet}");
        assert_eq!(match_snippet("abc", "zzz", 4), None);
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
