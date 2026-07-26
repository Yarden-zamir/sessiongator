use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    thread,
    time::Duration,
};

use crossterm::event::{self, Event, MouseButton, MouseEventKind};
use gator::layout::{binding_hint, binding_state_hint, help_line, HelpHint};
use gator::text::{insert_paste, list_window_offset, rect_contains};
use gator::{copy_to_clipboard, input_at_end, setup_terminal, AppResult};
use ratatui::{
    layout::Alignment,
    style::{Modifier, Style},
    text::Text,
    widgets::{Block, BorderType, Borders, List, ListState, Paragraph, Wrap},
};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::keybindings::{BindingContext, BindingTarget, CoreAction, Keymap};
use crate::model::{now_ms, sort_sessions, Session, SortMode, Tool};
use crate::search::{filter_sessions, SearchMode};
use crate::sources::{sources_from_env, Turn};
use crate::ui::{session_layout, session_list_items, transcript_text, Palette, Theme};

struct SessionBatch {
    sessions: Vec<Session>,
    error: Option<String>,
    done: bool,
}

struct TranscriptResult {
    key: String,
    turns: Result<Vec<Turn>, String>,
}

struct ContentEntry {
    key: String,
    text: String,
    done: bool,
}

/// Which panel receives arrow keys, mirroring the navigate implementation:
/// `Right` (with the input cursor at its end) moves into the transcript,
/// `Left` or `Up` at the top moves back to the list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Transcript,
}

/// The selection line handed to the shell wrapper.
fn resume_selection(session: &Session) -> String {
    format!(
        "resume\t{}\t{}\t{}",
        session.tool.name(),
        session.id,
        session.cwd
    )
}

fn resume_here_selection(session: &Session) -> String {
    format!("resume-here\t{}\t{}\t", session.tool.name(), session.id)
}

fn path_selection(session: &Session) -> String {
    format!("path\t{}", session.source_ref)
}

fn convert_selection(session: &Session) -> String {
    let target = session.tool.default_convert_target();
    format!(
        "convert\t{}\t{}\t{}\t{}",
        session.tool.name(),
        target.name(),
        session.id,
        session.cwd
    )
}

fn spawn_session_load(tx: mpsc::Sender<SessionBatch>) {
    thread::spawn(move || {
        let sources = sources_from_env();
        let available: Vec<_> = sources.iter().filter(|s| s.available()).collect();
        let total = available.len();
        if total == 0 {
            let _ = tx.send(SessionBatch {
                sessions: Vec::new(),
                error: Some(
                    "no session stores found (claude, opencode, codex, or copilot)".to_string(),
                ),
                done: true,
            });
            return;
        }
        for (index, source) in available.iter().enumerate() {
            let (sessions, error) = match source.list() {
                Ok(sessions) => (sessions, None),
                Err(message) => (
                    Vec::new(),
                    Some(format!("{}: {message}", source.tool().name())),
                ),
            };
            let _ = tx.send(SessionBatch {
                sessions,
                error,
                done: index + 1 == total,
            });
        }
    });
}

fn spawn_transcript_load(tool: Tool, id: String, key: String, tx: mpsc::Sender<TranscriptResult>) {
    thread::spawn(move || {
        let sources = sources_from_env();
        let turns = sources
            .iter()
            .find(|source| source.tool() == tool)
            .map(|source| source.transcript(&id))
            .unwrap_or_else(|| Err("source not found".to_string()));
        let _ = tx.send(TranscriptResult { key, turns });
    });
}

/// Extract lowercased transcript text for every session in the background so
/// "search all" can match message content. Streams one entry per session.
fn spawn_content_index(targets: Vec<(Tool, String, String)>, tx: mpsc::Sender<ContentEntry>) {
    thread::spawn(move || {
        let sources = sources_from_env();
        let total = targets.len();
        for (index, (tool, id, key)) in targets.into_iter().enumerate() {
            let text = sources
                .iter()
                .find(|source| source.tool() == tool)
                .and_then(|source| source.transcript(&id).ok())
                .map(|turns| {
                    turns
                        .iter()
                        .map(|turn| turn.text.to_lowercase())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let _ = tx.send(ContentEntry {
                key,
                text,
                done: index + 1 == total,
            });
        }
    });
}

pub fn select_session(theme: Theme, keymap: &Keymap) -> AppResult<Option<String>> {
    let palette = Palette::for_theme(theme);
    let (mut terminal, _guard) = setup_terminal()?;

    let mut input = Input::default();
    let mut sessions: Vec<Session> = Vec::new();
    let mut blobs: Vec<String> = Vec::new();
    let mut filtered: Vec<usize> = Vec::new();
    let mut selected = 0usize;
    let mut list_offset = 0usize;
    let mut sort_mode = SortMode::Updated;
    let mut search_mode = SearchMode::All;
    let mut focus = Focus::List;
    let mut loading = true;
    let mut error: Option<String> = None;
    // (session key, query) the transcript was last auto-scrolled for, so manual
    // scrolling afterwards is not overridden.
    let mut last_scroll_target: Option<(String, String)> = None;

    let mut transcripts: HashMap<String, Vec<Turn>> = HashMap::new();
    let mut transcript_errors: HashMap<String, String> = HashMap::new();
    let mut transcript_in_flight: HashSet<String> = HashSet::new();
    let mut transcript_scroll = 0usize;
    let mut transcript_max_scroll = 0usize;
    let mut transcript_page_step = 5usize;

    let mut content: HashMap<String, String> = HashMap::new();
    let mut indexing_started = false;
    let mut indexing_done = false;

    let (session_tx, session_rx) = mpsc::channel::<SessionBatch>();
    let (transcript_tx, transcript_rx) = mpsc::channel::<TranscriptResult>();
    let (content_tx, content_rx) = mpsc::channel::<ContentEntry>();
    spawn_session_load(session_tx);

    loop {
        let mut refilter = false;
        while let Ok(batch) = session_rx.try_recv() {
            if let Some(message) = batch.error {
                error = Some(message);
            }
            sessions.extend(batch.sessions);
            sort_sessions(&mut sessions, sort_mode);
            blobs = sessions.iter().map(Session::search_blob).collect();
            refilter = true;
            if batch.done {
                loading = false;
                if !indexing_started {
                    indexing_started = true;
                    let targets = sessions
                        .iter()
                        .map(|session| (session.tool, session.id.clone(), session.key()))
                        .collect();
                    spawn_content_index(targets, content_tx.clone());
                }
            }
        }
        while let Ok(entry) = content_rx.try_recv() {
            if !entry.text.is_empty() {
                content.insert(entry.key, entry.text);
            }
            if entry.done {
                indexing_done = true;
            }
            if search_mode == SearchMode::All && !input.value().trim().is_empty() {
                refilter = true;
            }
        }
        while let Ok(result) = transcript_rx.try_recv() {
            transcript_in_flight.remove(&result.key);
            match result.turns {
                Ok(turns) => {
                    transcripts.insert(result.key, turns);
                }
                Err(message) => {
                    transcript_errors.insert(result.key, message);
                }
            }
        }
        if refilter {
            let previous_key = filtered
                .get(selected)
                .and_then(|index| sessions.get(*index))
                .map(Session::key);
            filtered = filter_sessions(&sessions, &blobs, input.value(), search_mode, &content);
            selected = previous_key
                .and_then(|key| {
                    filtered
                        .iter()
                        .position(|index| sessions[*index].key() == key)
                })
                .unwrap_or_else(|| selected.min(filtered.len().saturating_sub(1)));
        }

        // Kick off a transcript load for the selected session if needed.
        if let Some(session) = filtered
            .get(selected)
            .and_then(|index| sessions.get(*index))
        {
            let key = session.key();
            if !transcripts.contains_key(&key)
                && !transcript_errors.contains_key(&key)
                && !transcript_in_flight.contains(&key)
            {
                transcript_in_flight.insert(key.clone());
                spawn_transcript_load(session.tool, session.id.clone(), key, transcript_tx.clone());
            }
        }

        let size = terminal.size()?;
        let ui = session_layout(size.into());
        let now = now_ms();
        terminal.draw(|frame| {
            let indexing_note = if indexing_started && !indexing_done {
                " indexing…"
            } else {
                ""
            };
            let list_title = if loading {
                format!("Sessions {} loading", sort_mode.label())
            } else {
                format!(
                    "Sessions {} {} {}/{}{indexing_note}",
                    search_mode.label(),
                    sort_mode.label(),
                    filtered.len(),
                    sessions.len()
                )
            };
            let list_border = if focus == Focus::List {
                palette.accent
            } else {
                palette.text
            };
            let left_block = Block::default()
                .borders(Borders::ALL)
                .title(format!("* {list_title}"))
                .border_style(Style::default().fg(list_border))
                .border_type(BorderType::Rounded);
            frame.render_widget(left_block, ui.left);

            let search = Paragraph::new(input.value())
                .style(Style::default().fg(palette.text))
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: false });
            frame.render_widget(search, ui.search);
            if focus == Focus::List {
                let cursor_x = input.visual_cursor().min(ui.search.width as usize);
                frame.set_cursor_position((ui.search.x + cursor_x as u16, ui.search.y));
            }

            let list_height = ui.results.height as usize;
            list_offset = list_window_offset(selected, list_offset, list_height, filtered.len());
            let items = session_list_items(
                &sessions,
                &filtered,
                list_offset,
                list_height,
                ui.results.width as usize,
                now,
                &palette,
            );
            let mut state = ListState::default();
            state.select(selected.checked_sub(list_offset));
            let list = List::new(items).highlight_style(
                Style::default()
                    .fg(ratatui::style::Color::Black)
                    .bg(palette.warm)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, ui.results, &mut state);

            let session = filtered
                .get(selected)
                .and_then(|index| sessions.get(*index));
            let key = session.map(Session::key);
            let turns = key.as_ref().and_then(|key| transcripts.get(key));
            let transcript_error = key
                .as_ref()
                .and_then(|key| transcript_errors.get(key))
                .map(String::as_str)
                .or(error.as_deref());
            // Highlight the query in the transcript in every search mode, so
            // moving between results always shows where the text matched.
            let highlight = Some(input.value()).filter(|value| !value.trim().is_empty());
            let transcript_width = ui.detail.width.saturating_sub(2) as usize;
            let (text, first_match) = transcript_text(
                session,
                turns.map(Vec::as_slice),
                transcript_error,
                key.as_ref()
                    .is_some_and(|key| transcript_in_flight.contains(key)),
                highlight,
                transcript_width,
                &palette,
            );
            let height = ui.detail.height.saturating_sub(2) as usize;
            transcript_page_step = height.max(1);
            transcript_max_scroll = text.lines.len().saturating_sub(height);
            transcript_scroll = transcript_scroll.min(transcript_max_scroll);
            // Auto-scroll to the first content match once per (session, query).
            let query = input.value().trim().to_string();
            if !query.is_empty() {
                if let (Some(key), Some(match_line)) = (key.clone(), first_match) {
                    let target = (key, query);
                    if last_scroll_target.as_ref() != Some(&target) {
                        transcript_scroll = match_line.saturating_sub(2).min(transcript_max_scroll);
                        last_scroll_target = Some(target);
                    }
                }
            }
            let title = session
                .map(|session| format!("{} {}", session.tool.glyph(), session.tool.name()))
                .unwrap_or_else(|| "Transcript".to_string());
            let transcript_border = if focus == Focus::Transcript {
                palette.accent
            } else {
                palette.text
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(transcript_border))
                .border_type(BorderType::Rounded);
            let widget = Paragraph::new(text)
                .block(block)
                .alignment(Alignment::Left)
                .scroll((transcript_scroll as u16, 0));
            frame.render_widget(widget, ui.detail);

            let help = help_line(&help_hints(keymap, search_mode, sort_mode), &palette);
            frame.render_widget(
                Paragraph::new(Text::from(help))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Keys")
                            .border_style(Style::default().fg(palette.muted))
                            .border_type(BorderType::Rounded),
                    )
                    .wrap(Wrap { trim: true }),
                ui.help,
            );
        })?;

        if event::poll(Duration::from_millis(100))? {
            // Drain every pending event before redrawing/refiltering so fast
            // typing costs one filter pass per frame, not one per keystroke
            // (content matching scans all indexed transcripts).
            let mut needs_refilter = false;
            let mut reset_selection = false;
            loop {
                match event::read()? {
                    Event::Key(key) => {
                        let context = match focus {
                            Focus::List => BindingContext::List,
                            Focus::Transcript => BindingContext::Transcript,
                        };
                        // Right only leaves the search input once the cursor
                        // reaches its end, so it still moves through typed text.
                        let resolved = match keymap.resolve(context, &key) {
                            Some(BindingTarget::Core(CoreAction::MoveRight))
                                if focus == Focus::List && !input_at_end(&input) =>
                            {
                                None
                            }
                            other => other.cloned(),
                        };
                        match resolved {
                            Some(BindingTarget::Core(action)) => match action {
                                CoreAction::Cancel => {
                                    terminal.show_cursor()?;
                                    return Ok(None);
                                }
                                CoreAction::Resume => {
                                    if let Some(session) = filtered
                                        .get(selected)
                                        .and_then(|index| sessions.get(*index))
                                    {
                                        terminal.show_cursor()?;
                                        return Ok(Some(resume_selection(session)));
                                    }
                                }
                                CoreAction::ResumeHere => {
                                    if let Some(session) = filtered
                                        .get(selected)
                                        .and_then(|index| sessions.get(*index))
                                    {
                                        terminal.show_cursor()?;
                                        return Ok(Some(resume_here_selection(session)));
                                    }
                                }
                                CoreAction::ShowPath => {
                                    if let Some(session) = filtered
                                        .get(selected)
                                        .and_then(|index| sessions.get(*index))
                                    {
                                        terminal.show_cursor()?;
                                        return Ok(Some(path_selection(session)));
                                    }
                                }
                                CoreAction::Convert => {
                                    if let Some(session) = filtered
                                        .get(selected)
                                        .and_then(|index| sessions.get(*index))
                                    {
                                        terminal.show_cursor()?;
                                        return Ok(Some(convert_selection(session)));
                                    }
                                }
                                CoreAction::CopyId => {
                                    if let Some(session) = filtered
                                        .get(selected)
                                        .and_then(|index| sessions.get(*index))
                                    {
                                        let _ = copy_to_clipboard(&session.id);
                                    }
                                }
                                CoreAction::ToggleSearch => {
                                    search_mode = search_mode.toggle();
                                    needs_refilter = true;
                                }
                                CoreAction::CycleSort => {
                                    sort_mode = sort_mode.next();
                                    let previous_key = filtered
                                        .get(selected)
                                        .and_then(|index| sessions.get(*index))
                                        .map(Session::key);
                                    sort_sessions(&mut sessions, sort_mode);
                                    blobs = sessions.iter().map(Session::search_blob).collect();
                                    filtered = filter_sessions(
                                        &sessions,
                                        &blobs,
                                        input.value(),
                                        search_mode,
                                        &content,
                                    );
                                    selected = previous_key
                                        .and_then(|key| {
                                            filtered
                                                .iter()
                                                .position(|index| sessions[*index].key() == key)
                                        })
                                        .unwrap_or(0);
                                    list_offset = 0;
                                }
                                CoreAction::MoveUp => match focus {
                                    Focus::List => {
                                        selected = selected.saturating_sub(1);
                                        transcript_scroll = 0;
                                    }
                                    // At the top of the transcript, Up returns
                                    // focus to the list.
                                    Focus::Transcript => {
                                        if transcript_scroll > 0 {
                                            transcript_scroll -= 1;
                                        } else {
                                            focus = Focus::List;
                                        }
                                    }
                                },
                                CoreAction::MoveDown => match focus {
                                    Focus::List => {
                                        selected =
                                            (selected + 1).min(filtered.len().saturating_sub(1));
                                        transcript_scroll = 0;
                                    }
                                    Focus::Transcript => {
                                        transcript_scroll =
                                            (transcript_scroll + 1).min(transcript_max_scroll);
                                    }
                                },
                                CoreAction::MoveLeft => focus = Focus::List,
                                CoreAction::MoveRight => focus = Focus::Transcript,
                                CoreAction::MoveHome => {
                                    selected = 0;
                                    transcript_scroll = 0;
                                }
                                CoreAction::MoveEnd => {
                                    selected = filtered.len().saturating_sub(1);
                                    transcript_scroll = 0;
                                }
                                CoreAction::PageUp => {
                                    transcript_scroll =
                                        transcript_scroll.saturating_sub(transcript_page_step);
                                }
                                CoreAction::PageDown => {
                                    transcript_scroll = (transcript_scroll + transcript_page_step)
                                        .min(transcript_max_scroll);
                                }
                                CoreAction::ScrollTop => transcript_scroll = 0,
                                CoreAction::ScrollBottom => {
                                    transcript_scroll = transcript_max_scroll
                                }
                            },
                            // Disabled and configured targets consume the key
                            // without typing it.
                            Some(_) => {}
                            None => {
                                if focus == Focus::List {
                                    let before = input.value().to_string();
                                    let _ = input.handle_event(&Event::Key(key));
                                    if input.value() != before {
                                        needs_refilter = true;
                                        reset_selection = true;
                                    }
                                }
                            }
                        }
                    }
                    Event::Paste(value) => {
                        insert_paste(&mut input, &value);
                        needs_refilter = true;
                        reset_selection = true;
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if rect_contains(ui.results, mouse.column, mouse.row) {
                                let row = mouse.row.saturating_sub(ui.results.y) as usize;
                                selected =
                                    (list_offset + row).min(filtered.len().saturating_sub(1));
                                transcript_scroll = 0;
                                focus = Focus::List;
                            } else if rect_contains(ui.detail, mouse.column, mouse.row) {
                                focus = Focus::Transcript;
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            if rect_contains(ui.detail, mouse.column, mouse.row) {
                                transcript_scroll = transcript_scroll.saturating_sub(1);
                            } else if rect_contains(ui.results, mouse.column, mouse.row) {
                                selected = selected.saturating_sub(1);
                                transcript_scroll = 0;
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if rect_contains(ui.detail, mouse.column, mouse.row) {
                                transcript_scroll =
                                    (transcript_scroll + 1).min(transcript_max_scroll);
                            } else if rect_contains(ui.results, mouse.column, mouse.row) {
                                selected = (selected + 1).min(filtered.len().saturating_sub(1));
                                transcript_scroll = 0;
                            }
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => {}
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            if needs_refilter {
                let previous_key = filtered
                    .get(selected)
                    .and_then(|index| sessions.get(*index))
                    .map(Session::key);
                filtered = filter_sessions(&sessions, &blobs, input.value(), search_mode, &content);
                selected = if reset_selection {
                    0
                } else {
                    previous_key
                        .and_then(|key| {
                            filtered
                                .iter()
                                .position(|index| sessions[*index].key() == key)
                        })
                        .unwrap_or_else(|| selected.min(filtered.len().saturating_sub(1)))
                };
                list_offset = 0;
                transcript_scroll = 0;
            }
        }
    }
}

/// Footer hints, labeled with whatever chord is currently bound to each
/// action, so rebinding a key updates the help automatically.
fn help_hints(keymap: &Keymap, search_mode: SearchMode, sort_mode: SortMode) -> Vec<HelpHint> {
    let context = BindingContext::List;
    [
        binding_hint(keymap, context, CoreAction::Resume, "resume"),
        binding_hint(keymap, context, CoreAction::ResumeHere, "resume here"),
        binding_state_hint(
            keymap,
            context,
            CoreAction::ToggleSearch,
            format!("search:{}", search_mode.label()),
        ),
        binding_state_hint(
            keymap,
            context,
            CoreAction::CycleSort,
            format!("sort:{}", sort_mode.label()),
        ),
        binding_hint(keymap, context, CoreAction::ShowPath, "path"),
        binding_hint(keymap, context, CoreAction::Convert, "convert default"),
        binding_hint(keymap, context, CoreAction::CopyId, "copy id"),
        binding_hint(keymap, context, CoreAction::MoveRight, "transcript"),
        binding_hint(keymap, context, CoreAction::Cancel, "quit"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tool;

    fn session(tool: Tool, id: &str, cwd: &str) -> Session {
        Session {
            tool,
            id: id.to_string(),
            title: "T".to_string(),
            cwd: cwd.to_string(),
            created_ms: 0,
            updated_ms: 0,
            message_count: 0,
            model: None,
            source_ref: "/store/ref".to_string(),
            extras: Vec::new(),
        }
    }

    #[test]
    fn selection_lines() {
        let claude = session(Tool::Claude, "abc-123", "/Users/me/proj");
        assert_eq!(
            resume_selection(&claude),
            "resume\tclaude\tabc-123\t/Users/me/proj"
        );
        assert_eq!(
            resume_here_selection(&claude),
            "resume-here\tclaude\tabc-123\t"
        );
        let opencode = session(Tool::Opencode, "ses_1", "/w");
        assert_eq!(resume_selection(&opencode), "resume\topencode\tses_1\t/w");
        assert_eq!(path_selection(&claude), "path\t/store/ref");
        assert_eq!(
            convert_selection(&claude),
            "convert\tclaude\topencode\tabc-123\t/Users/me/proj"
        );
        assert_eq!(
            convert_selection(&opencode),
            "convert\topencode\tclaude\tses_1\t/w"
        );
        let codex = session(Tool::Codex, "codex_1", "/c");
        assert_eq!(resume_selection(&codex), "resume\tcodex\tcodex_1\t/c");
        assert_eq!(
            convert_selection(&codex),
            "convert\tcodex\tclaude\tcodex_1\t/c"
        );
        let copilot = session(Tool::Copilot, "copilot_1", "/p");
        assert_eq!(resume_selection(&copilot), "resume\tcopilot\tcopilot_1\t/p");
        assert_eq!(
            convert_selection(&copilot),
            "convert\tcopilot\tclaude\tcopilot_1\t/p"
        );
    }

    #[test]
    fn window_offset_follows_selection() {
        assert_eq!(list_window_offset(0, 0, 10, 100), 0);
        assert_eq!(list_window_offset(15, 0, 10, 100), 6);
        assert_eq!(list_window_offset(3, 6, 10, 100), 3);
        assert_eq!(list_window_offset(0, 0, 0, 0), 0);
    }
}
