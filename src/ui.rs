use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::ListItem,
};

use crate::model::{rel_time, shorten_home, Session};
use crate::sources::Turn;
use gator::text::{highlight_line as highlighted_line, wrap_text_line};

// The theme (Theme + Palette + OS dark-mode detection) is shared across the
// gator app family.
pub use gator::theme::{Palette, Theme};

/// The shared two-pane shell, with the session list taking 55% of the width.
pub fn session_layout(size: Rect) -> gator::layout::SplitLayout {
    gator::layout::split_layout(size, 55)
}

#[allow(clippy::too_many_arguments)]
pub fn session_list_items(
    sessions: &[Session],
    filtered: &[usize],
    offset: usize,
    height: usize,
    width: usize,
    now_ms: i64,
    palette: &Palette,
    separator_at: Option<usize>,
) -> Vec<ListItem<'static>> {
    if filtered.is_empty() || height == 0 {
        return vec![ListItem::new(Line::from(Span::styled(
            "No sessions",
            Style::default().fg(palette.muted),
        )))];
    }
    let visual_len = filtered.len() + usize::from(separator_at.is_some());
    let end = (offset + height).min(visual_len);
    (offset..end)
        .filter_map(|visual_index| {
            if separator_at == Some(visual_index) {
                return Some(ListItem::new(Line::from(Span::styled(
                    session_separator_label(),
                    Style::default().fg(palette.muted),
                ))));
            }
            let logical_index = visual_index
                - usize::from(separator_at.is_some_and(|separator| visual_index > separator));
            filtered
                .get(logical_index)
                .and_then(|index| sessions.get(*index))
                .map(|session| session_row(session, width, now_ms, palette))
        })
        .collect()
}

fn session_separator_label() -> &'static str {
    "  ── other sessions"
}

fn session_row(
    session: &Session,
    width: usize,
    now_ms: i64,
    palette: &Palette,
) -> ListItem<'static> {
    let when = rel_time(session.updated_ms, now_ms);
    let cwd = shorten_home(&session.cwd);
    let prefix = format!("{} {:>4}  ", session.tool.glyph(), when);
    let mut tail = format!("{}m", session.message_count);
    if let Some(model) = &session.model {
        tail.push(' ');
        tail.push_str(model);
    }
    let prefix_len = prefix.chars().count();
    let tail_len = tail.chars().count();
    let cwd_budget = 28usize;
    let cwd_text = truncate_with_ellipsis(&cwd, cwd_budget);
    let cwd_rendered = format!("{cwd_text:<cwd_budget$}  ");
    let title_budget =
        width.saturating_sub(prefix_len + cwd_rendered.chars().count() + tail_len + 1);
    let title = truncate_with_ellipsis(session.title.trim(), title_budget);
    let used = prefix_len + cwd_rendered.chars().count() + title.chars().count() + tail_len;
    let padding = width.saturating_sub(used);
    ListItem::new(Line::from(vec![
        Span::styled(prefix, Style::default().fg(palette.muted)),
        Span::styled(cwd_rendered, Style::default().fg(palette.accent)),
        Span::styled(title, Style::default().fg(palette.text)),
        Span::raw(" ".repeat(padding)),
        Span::styled(tail, Style::default().fg(palette.key)),
    ]))
}

pub use gator::text::truncate_with_ellipsis;

/// Transcript panel text: metadata header + role-labeled turns. When
/// `highlight` is set, occurrences are shown in reverse video and the index of
/// the first matching line is returned so the caller can scroll to it.
#[allow(clippy::too_many_arguments)]
pub fn transcript_text(
    session: Option<&Session>,
    turns: Option<&[Turn]>,
    error: Option<&str>,
    loading: bool,
    highlight: Option<&str>,
    wrap_width: usize,
    palette: &Palette,
) -> (Text<'static>, Option<usize>) {
    let mut lines: Vec<Line> = Vec::new();
    let wrap_width = wrap_width.max(1);
    let Some(session) = session else {
        push_wrapped_styled(
            &mut lines,
            "No session selected",
            Style::default().fg(palette.muted),
            wrap_width,
        );
        return (Text::from(lines), None);
    };

    push_wrapped_styled(
        &mut lines,
        session.title.clone(),
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
        wrap_width,
    );
    push_wrapped_styled(
        &mut lines,
        format!("{} · {}", session.tool.name(), shorten_home(&session.cwd)),
        Style::default().fg(palette.muted),
        wrap_width,
    );
    let mut meta = Vec::new();
    if let Some(model) = &session.model {
        meta.push(model.clone());
    }
    meta.push(format!("{} msgs", session.message_count));
    meta.push(crate::model::format_utc(session.updated_ms));
    for (key, value) in &session.extras {
        meta.push(format!("{key}={value}"));
    }
    push_wrapped_styled(
        &mut lines,
        meta.join(" · "),
        Style::default().fg(palette.muted),
        wrap_width,
    );
    lines.push(Line::default());

    if let Some(error) = error {
        push_wrapped_styled(
            &mut lines,
            format!("error: {error}"),
            Style::default().fg(Color::Red),
            wrap_width,
        );
        return (Text::from(lines), None);
    }
    let Some(turns) = turns else {
        push_wrapped_styled(
            &mut lines,
            if loading { "Loading transcript…" } else { "" },
            Style::default().fg(palette.muted),
            wrap_width,
        );
        return (Text::from(lines), None);
    };
    if turns.is_empty() {
        push_wrapped_styled(
            &mut lines,
            "(empty session)",
            Style::default().fg(palette.muted),
            wrap_width,
        );
        return (Text::from(lines), None);
    }

    let needle = highlight
        .map(str::trim)
        .filter(|needle| !needle.is_empty())
        .map(str::to_lowercase);
    let mut first_match: Option<usize> = None;
    for turn in turns {
        let role_color = if turn.role == "user" {
            Color::Rgb(64, 160, 96)
        } else {
            palette.warm
        };
        lines.push(Line::from(Span::styled(
            format!("▎ {}", turn.role),
            Style::default().fg(role_color).add_modifier(Modifier::BOLD),
        )));
        for raw_line in turn.text.lines() {
            for wrapped in wrap_text_line(raw_line, wrap_width) {
                if first_match.is_none() {
                    if let Some(needle) = &needle {
                        if wrapped.to_lowercase().contains(needle) {
                            first_match = Some(lines.len());
                        }
                    }
                }
                lines.push(highlighted_line(&wrapped, highlight, palette.text));
            }
        }
        lines.push(Line::default());
    }
    (Text::from(lines), first_match)
}

fn push_wrapped_styled(
    lines: &mut Vec<Line<'static>>,
    value: impl AsRef<str>,
    style: Style,
    width: usize,
) {
    for line in value.as_ref().lines() {
        for wrapped in wrap_text_line(line, width) {
            lines.push(Line::from(Span::styled(wrapped, style)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> Session {
        use crate::model::Tool;
        Session {
            tool: Tool::Claude,
            id: id.to_string(),
            title: id.to_string(),
            cwd: "/work/project".to_string(),
            created_ms: 0,
            updated_ms: 0,
            message_count: 0,
            model: None,
            source_ref: String::new(),
            extras: Vec::new(),
        }
    }

    // Theme parsing, palette values, truncation, and match highlighting are
    // gator's contracts and are tested there.

    #[test]
    fn transcript_reports_first_match_line() {
        use crate::model::{Session, Tool};
        let session = Session {
            tool: Tool::Claude,
            id: "x".to_string(),
            title: "T".to_string(),
            cwd: "/w".to_string(),
            created_ms: 0,
            updated_ms: 0,
            message_count: 2,
            model: None,
            source_ref: String::new(),
            extras: Vec::new(),
        };
        let turns = vec![
            Turn {
                role: "user".to_string(),
                text: "first line\nsecond line".to_string(),
            },
            Turn {
                role: "assistant".to_string(),
                text: "the Needle is here".to_string(),
            },
        ];
        let palette = Palette::for_theme(Theme::Light);
        let (text, first) = transcript_text(
            Some(&session),
            Some(&turns),
            None,
            false,
            Some("needle"),
            80,
            &palette,
        );
        let line = first.expect("match line");
        // the reported index points at the line containing the needle
        let rendered: String = text.lines[line]
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(rendered.to_lowercase().contains("needle"));
        // no highlight → no match index
        let (_, none) = transcript_text(
            Some(&session),
            Some(&turns),
            None,
            false,
            None,
            80,
            &palette,
        );
        assert_eq!(none, None);
    }

    #[test]
    fn transcript_match_line_counts_wrapped_rows() {
        use crate::model::{Session, Tool};
        let session = Session {
            tool: Tool::Claude,
            id: "x".to_string(),
            title: "T".to_string(),
            cwd: "/w".to_string(),
            created_ms: 0,
            updated_ms: 0,
            message_count: 1,
            model: None,
            source_ref: String::new(),
            extras: Vec::new(),
        };
        let turns = vec![Turn {
            role: "assistant".to_string(),
            text: "abcdefghijNEEDLE".to_string(),
        }];
        let palette = Palette::for_theme(Theme::Light);
        let (text, first) = transcript_text(
            Some(&session),
            Some(&turns),
            None,
            false,
            Some("needle"),
            10,
            &palette,
        );
        let line = first.expect("wrapped match line");
        let rendered: String = text.lines[line]
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(rendered.to_lowercase().contains("needle"));
        assert!(line > 5, "match should account for wrapped rows");
    }

    #[test]
    fn session_list_renders_separator_between_groups() {
        let sessions = vec![session("project"), session("other")];
        let palette = Palette::for_theme(Theme::Light);
        let items = session_list_items(&sessions, &[0, 1], 0, 3, 80, 0, &palette, Some(1));
        assert_eq!(items.len(), 3);
        assert_eq!(session_separator_label(), "  ── other sessions");
    }
}
