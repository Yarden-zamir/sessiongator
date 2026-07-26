mod model;
mod native_import;
mod search;
mod session;
mod sources;
mod ui;

use gator::{ensure_tty_stdin, write_selection, AppResult};

use crate::model::{now_ms, rel_time, shorten_home, sort_sessions, SortMode};
use crate::sources::sources_from_env;
use crate::ui::Theme;

const USAGE: &str = "Usage: sessiongator [--list] [--theme <auto|light|dark>]
       sessiongator convert --id <id> --from <claude|opencode|codex|copilot> --to <claude|opencode|codex|copilot> [options]

Browse, search, and resume Claude Code, opencode, Codex, and Copilot sessions.

  (no args)   interactive picker; prints a selection line on Enter:
              resume\\t<tool>\\t<id>\\t<cwd>   (Enter)
              resume-here\\t<tool>\\t<id>\\t      (Ctrl+Enter)
              path\\t<source path>           (Ctrl+O)
  --list      print all sessions, newest first (tool, id, age, cwd, title)
  --theme     interactive color theme; defaults to SESSIONGATOR_THEME or auto
  convert     convert one session into the target tool's native store
  -h, --help  show this help";

struct UiOptions {
    list: bool,
    theme: Option<Theme>,
}

fn main() -> AppResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "convert") {
        return native_import::run_convert(&args[1..]);
    }
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let options = match parse_ui_options(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            std::process::exit(2);
        }
    };
    if options.list {
        return list_mode();
    }
    let theme = resolve_theme(
        options.theme,
        std::env::var("SESSIONGATOR_THEME").ok().as_deref(),
    )
    .map_err(|message| format!("SESSIONGATOR_THEME: {message}"))?;
    ensure_tty_stdin()?;
    match session::select_session(theme)? {
        Some(selection) => write_selection(&selection),
        None => std::process::exit(1),
    }
}

fn parse_ui_options(args: &[String]) -> Result<UiOptions, String> {
    let mut list = false;
    let mut theme = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--list" => list = true,
            "--theme" => {
                if theme.is_some() {
                    return Err("--theme may only be passed once".to_string());
                }
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--theme requires a value".to_string())?;
                theme = Some(Theme::parse(value)?);
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }
    Ok(UiOptions { list, theme })
}

fn resolve_theme(cli: Option<Theme>, environment: Option<&str>) -> Result<Theme, String> {
    if let Some(theme) = cli {
        return Ok(theme);
    }
    environment.map(Theme::parse).unwrap_or(Ok(Theme::Auto))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_theme_option() {
        let options = parse_ui_options(&["--theme".to_string(), "dark".to_string()]).unwrap();
        assert!(!options.list);
        assert_eq!(options.theme, Some(Theme::Dark));
        assert!(parse_ui_options(&["--theme".to_string()]).is_err());
        assert!(parse_ui_options(&[
            "--theme".to_string(),
            "dark".to_string(),
            "--theme".to_string(),
            "light".to_string(),
        ])
        .is_err());
    }

    #[test]
    fn cli_theme_overrides_environment() {
        assert_eq!(
            resolve_theme(Some(Theme::Light), Some("dark")),
            Ok(Theme::Light)
        );
        assert_eq!(resolve_theme(None, Some("dark")), Ok(Theme::Dark));
        assert_eq!(resolve_theme(None, None), Ok(Theme::Auto));
        assert!(resolve_theme(None, Some("system")).is_err());
    }
}
