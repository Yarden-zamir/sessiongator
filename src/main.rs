mod model;
mod search;
mod session;
mod sources;
mod ui;

use gator::{ensure_tty_stdin, write_selection, AppResult};

use crate::model::{now_ms, rel_time, shorten_home, sort_sessions, SortMode};
use crate::sources::sources_from_env;

const USAGE: &str = "Usage: sessiongator [--list]

Browse, search, and resume Claude Code and opencode sessions.

  (no args)   interactive picker; prints a selection line on Enter:
              resume\\t<tool>\\t<id>\\t<cwd>   (Enter)
              path\\t<source path>           (Ctrl+O)
  --list      print all sessions, newest first (tool, id, age, cwd, title)
  -h, --help  show this help";

fn main() -> AppResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--list") {
        return list_mode();
    }
    if let Some(unknown) = args.first() {
        eprintln!("unknown argument: {unknown}\n{USAGE}");
        std::process::exit(2);
    }
    ensure_tty_stdin()?;
    match session::select_session()? {
        Some(selection) => write_selection(&selection),
        None => std::process::exit(1),
    }
}

fn list_mode() -> AppResult<()> {
    let mut sessions = Vec::new();
    let mut errors = Vec::new();
    for source in sources_from_env() {
        if !source.available() {
            continue;
        }
        match source.list() {
            Ok(batch) => sessions.extend(batch),
            Err(message) => errors.push(format!("{}: {message}", source.tool().name())),
        }
    }
    sort_sessions(&mut sessions, SortMode::Updated);
    let now = now_ms();
    for session in &sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            session.tool.name(),
            session.id,
            rel_time(session.updated_ms, now),
            shorten_home(&session.cwd),
            session.title
        );
    }
    for error in &errors {
        eprintln!("warning: {error}");
    }
    if sessions.is_empty() && !errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}
