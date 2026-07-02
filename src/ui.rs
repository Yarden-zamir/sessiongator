use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, ListItem},
};

use crate::model::{rel_time, shorten_home, Session};
use crate::sources::Turn;

pub struct Palette {
    pub accent: Color,
    pub warm: Color,
    pub text: Color,
    pub muted: Color,
    pub key: Color,
}

impl Palette {
    pub fn default_palette() -> Self {
        Self {
            accent: Color::Rgb(72, 166, 255),
            warm: Color::Rgb(255, 181, 92),
            text: Color::Black,
            muted: Color::Black,
            key: Color::Rgb(150, 150, 150),
        }
    }
}

pub struct SessionLayout {
    pub left: Rect,
    pub search: Rect,
    pub results: Rect,
    pub transcript: Rect,
    pub help: Rect,
}

pub fn session_layout(size: Rect) -> SessionLayout {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(3)])
        .split(size);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(chunks[0]);
    let left_inner = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .inner(body[0]);
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(left_inner);
    SessionLayout {
        left: body[0],
        search: left_chunks[0],
        results: left_chunks[1],
        transcript: body[1],
        help: chunks[1],
    }
}

pub fn session_list_items(
    sessions: &[Session],
    filtered: &[usize],
    offset: usize,
    height: usize,
    width: usize,
    now_ms: i64,
    palette: &Palette,
) -> Vec<ListItem<'static>> {
    if filtered.is_empty() || height == 0 {
        return vec![ListItem::new(Line::from(Span::styled(
            "No sessions",
            Style::default().fg(palette.muted),
        )))];
    }
    let end = (offset + height).min(filtered.len());
    filtered[offset..end]
        .iter()
        .filter_map(|index| sessions.get(*index))
        .map(|session| session_row(session, width, now_ms, palette))
        .collect()
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

pub fn truncate_with_ellipsis(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = value.chars().count();
    if count <= max {
        return value.to_string();
    }
    if max <= 1 {
        return value.chars().take(max).collect();
    }
    let trimmed = value.chars().take(max - 1).collect::<String>();
    format!("{trimmed}…")
}

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
    palette: &Palette,
) -> (Text<'static>, Option<usize>) {
    let mut lines: Vec<Line> = Vec::new();
    let Some(session) = session else {
        lines.push(Line::from(Span::styled(
            "No session selected",
            Style::default().fg(palette.muted),
        )));
        return (Text::from(lines), None);
    };

    lines.push(Line::from(Span::styled(
        session.title.clone(),
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("{} · {}", session.tool.name(), shorten_home(&session.cwd)),
        Style::default().fg(palette.muted),
    )));
    let mut meta = Vec::new();
    if let Some(model) = &session.model {
        meta.push(model.clone());
    }
    meta.push(format!("{} msgs", session.message_count));
    meta.push(crate::model::format_utc(session.updated_ms));
    for (key, value) in &session.extras {
        meta.push(format!("{key}={value}"));
    }
    lines.push(Line::from(Span::styled(
        meta.join(" · "),
        Style::default().fg(palette.muted),
    )));
    lines.push(Line::default());

    if let Some(error) = error {
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Red),
        )));
        return (Text::from(lines), None);
    }
    let Some(turns) = turns else {
        lines.push(Line::from(Span::styled(
            if loading { "Loading transcript…" } else { "" }.to_string(),
            Style::default().fg(palette.muted),
        )));
        return (Text::from(lines), None);
    };
    if turns.is_empty() {
        lines.push(Line::from(Span::styled(
            "(empty session)".to_string(),
            Style::default().fg(palette.muted),
        )));
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
            if first_match.is_none() {
                if let Some(needle) = &needle {
                    if raw_line.to_lowercase().contains(needle) {
                        first_match = Some(lines.len());
                    }
                }
            }
            lines.push(highlighted_line(raw_line, highlight, palette.text));
        }
        lines.push(Line::default());
    }
    (Text::from(lines), first_match)
}

/// Split a line into spans, rendering case-insensitive `needle` matches in
/// reverse video.
fn highlighted_line(line: &str, needle: Option<&str>, text_color: Color) -> Line<'static> {
    let base = Style::default().fg(text_color);
    let Some(needle) = needle.map(str::trim).filter(|needle| !needle.is_empty()) else {
        return Line::from(Span::styled(line.to_string(), base));
    };
    let lower_line = line.to_lowercase();
    let lower_needle = needle.to_lowercase();
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = lower_line[cursor..].find(&lower_needle) {
        let start = cursor + found;
        let end = start + lower_needle.len();
        // Guard against multi-byte boundaries: fall back to no highlight if the
        // lowercased offsets don't align with the original string.
        if !line.is_char_boundary(start) || !line.is_char_boundary(end) || end > line.len() {
            return Line::from(Span::styled(line.to_string(), base));
        }
        if start > cursor {
            spans.push(Span::styled(line[cursor..start].to_string(), base));
        }
        spans.push(Span::styled(
            line[start..end].to_string(),
            base.add_modifier(Modifier::REVERSED),
        ));
        cursor = end;
    }
    if cursor < line.len() {
        spans.push(Span::styled(line[cursor..].to_string(), base));
    }
    if spans.is_empty() {
        return Line::from(Span::styled(line.to_string(), base));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_with_ellipsis("hello world", 6), "hello…");
        assert_eq!(truncate_with_ellipsis("hello", 0), "");
    }

    #[test]
    fn highlight_marks_matches() {
        let line = highlighted_line("the Rate limiter rate", Some("rate"), Color::Black);
        let reversed: Vec<String> = line
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(reversed, vec!["Rate", "rate"]);
    }

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
        let palette = Palette::default_palette();
        let (text, first) = transcript_text(
            Some(&session),
            Some(&turns),
            None,
            false,
            Some("needle"),
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
        let (_, none) = transcript_text(Some(&session), Some(&turns), None, false, None, &palette);
        assert_eq!(none, None);
    }

    #[test]
    fn highlight_survives_multibyte() {
        // must not panic or split a char boundary
        let _ = highlighted_line("préfix — ünïcode", Some("é"), Color::Black);
        let _ = highlighted_line("emoji 🎉 test", Some("test"), Color::Black);
    }
}
