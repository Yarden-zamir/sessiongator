mod cli;
mod config;
mod convert;
mod history;
mod keybindings;
mod model;
mod native;
mod search;
mod session;
mod stores;
mod time;
mod trash;
mod ui;

use clap::Parser;
use gator::{ensure_tty_stdin, write_selection, AppResult};

use crate::cli::Cli;
use crate::ui::Theme;

fn main() -> AppResult<()> {
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        if let Err(error) = cli::run(command) {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let loaded = config::load_config(&[])?;
    let theme = resolve_theme(
        cli.theme,
        std::env::var("SESSIONGATOR_THEME").ok().as_deref(),
        loaded.theme,
    )
    .map_err(|message| format!("SESSIONGATOR_THEME: {message}"))?;
    ensure_tty_stdin()?;
    match session::select_session(theme, &loaded.keymap, cli.query.as_deref().unwrap_or(""))? {
        Some(selection) => write_selection(&selection),
        None => std::process::exit(1),
    }
}

/// Theme precedence: `--theme` beats `SESSIONGATOR_THEME`, which beats the
/// config file, which falls back to `auto`.
fn resolve_theme(
    cli: Option<Theme>,
    environment: Option<&str>,
    configured: Option<Theme>,
) -> Result<Theme, String> {
    if let Some(theme) = cli {
        return Ok(theme);
    }
    if let Some(value) = environment {
        return Theme::parse(value);
    }
    Ok(configured.unwrap_or(Theme::Auto))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_theme_overrides_environment_which_overrides_config() {
        assert_eq!(
            resolve_theme(Some(Theme::Light), Some("dark"), Some(Theme::Dark)),
            Ok(Theme::Light)
        );
        assert_eq!(
            resolve_theme(None, Some("dark"), Some(Theme::Light)),
            Ok(Theme::Dark)
        );
        assert_eq!(
            resolve_theme(None, None, Some(Theme::Dark)),
            Ok(Theme::Dark)
        );
        assert_eq!(resolve_theme(None, None, None), Ok(Theme::Auto));
        assert!(resolve_theme(None, Some("system"), None).is_err());
    }
}
