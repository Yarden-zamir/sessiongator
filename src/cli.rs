use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command as Process,
    sync::Mutex,
};

use clap::{Args, CommandFactory, Parser, Subcommand};
use gator::AppResult;
use serde_json::{json, Value};

use crate::convert::versions::{describe, detect_cli_version, support, Role};
use crate::convert::{run_convert, ConvertArgs};
use crate::history::{Action, History, SessionRef};
use crate::model::{resume_line, shorten_home, sort_sessions, Session, SortMode, Tool};
use crate::native::Turn;
use crate::search::{filter_sessions, match_snippet, resolve_session, SearchMode};
use crate::session::current_project_root;
use crate::stores::{load_sessions, read_transcripts, store_for};
use crate::time::{format_utc, now_ms, rel_time};
use crate::trash::trash;
use crate::ui::Theme;

const PICKER_HELP: &str =
    "Without a command, open the interactive picker. On Enter it prints a selection line:
  resume\\t<tool>\\t<id>\\t<cwd>        Enter
  resume-here\\t<tool>\\t<id>\\t        Ctrl+Enter
  path\\t<source path>                 Ctrl+O
  convert\\t<from>\\t<to>\\t<id>\\t<cwd>  Ctrl+T

A <SESSION> is an id, a unique id prefix, tool:id, or text that matches one
session's title, path, or model. Without one, the newest session in the
current project is used.

Examples:
  sessiongator list rate limiter --here
  sessiongator search 'connection pool' --tool claude -n 5
  sessiongator show e4255e84 --grep migration
  sessiongator resume 'rate limiter'
  sessiongator convert e4255e84 --to codex --mark-source
  sessiongator undo";

#[derive(Parser)]
#[command(
    name = "sessiongator",
    version,
    about = "Browse, search, resume, and convert Claude Code, opencode, Codex, and Copilot sessions.",
    after_help = PICKER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Picker color theme. Overrides SESSIONGATOR_THEME and the config file.
    #[arg(long, value_parser = Theme::parse)]
    pub theme: Option<Theme>,
    /// Open the picker with this search text.
    #[arg(short, long)]
    pub query: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// List sessions, newest first, filtered by title, path, and model.
    #[command(visible_alias = "ls")]
    List {
        /// Text that every match contains (title, path, or model).
        query: Vec<String>,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Search sessions like the picker: transcript text, title, path, or model.
    #[command(visible_alias = "s")]
    Search {
        /// Text to find.
        #[arg(required = true)]
        query: Vec<String>,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Print a session transcript.
    Show {
        #[command(flatten)]
        target: SessionArgs,
        /// Only print turns that contain this text.
        #[arg(short, long)]
        grep: Option<String>,
        /// Print readable text instead of JSON.
        #[arg(long)]
        text: bool,
    },
    /// Resume a session with its tool, in the session directory.
    #[command(visible_alias = "r")]
    Resume {
        #[command(flatten)]
        target: SessionArgs,
        /// Stay in the current directory.
        #[arg(long)]
        here: bool,
        /// Print the shell command instead of running it.
        #[arg(long)]
        print: bool,
    },
    /// Print the session store path.
    Path {
        #[command(flatten)]
        target: SessionArgs,
    },
    /// Convert one session into another tool's native store.
    Convert(ConvertArgs),
    /// Move a session to the sessiongator trash. `undo` restores it.
    #[command(visible_alias = "rm")]
    Delete {
        #[command(flatten)]
        target: SessionArgs,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Undo a conversion or a delete. Default: the newest operation.
    Undo {
        /// Operation id from `sessiongator history`.
        operation: Option<String>,
        /// Also move the session that a conversion created to the trash.
        #[arg(long)]
        target: bool,
    },
    /// List conversions and deletes that `undo` can revert.
    History {
        /// Print at most N operations, newest first.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Print tab-separated text instead of one JSON object per line.
        #[arg(long)]
        text: bool,
    },
    /// Show the installed tool versions and their conversion support.
    #[command(visible_alias = "versions")]
    Compat,
    /// Print the JSON Schema for the config file.
    ConfigSchema,
    /// Print a shell completion script.
    Completions { shell: clap_complete::Shell },
}

#[derive(Args)]
pub struct FilterArgs {
    /// Only sessions of this tool. Repeat for more tools.
    #[arg(short, long = "tool", value_enum)]
    tools: Vec<Tool>,
    /// Only sessions in the current project (git root of the current directory).
    #[arg(short = 'p', long, conflicts_with = "dir")]
    here: bool,
    /// Only sessions whose directory is DIR or below it.
    #[arg(short, long)]
    dir: Option<PathBuf>,
    /// Sort order.
    #[arg(short, long, value_enum, default_value = "updated")]
    sort: SortMode,
    /// Print at most N sessions.
    #[arg(short = 'n', long)]
    limit: Option<usize>,
    /// Print tab-separated columns instead of one JSON object per line.
    #[arg(long)]
    text: bool,
}

#[derive(Args)]
pub struct SessionArgs {
    /// Session id, unique id prefix, tool:id, or search text.
    session: Option<String>,
    /// Only look at sessions of this tool. Repeat for more tools.
    #[arg(short, long = "tool", value_enum)]
    tools: Vec<Tool>,
}

pub fn run(command: Command) -> AppResult<()> {
    let result = match command {
        Command::List { query, filter } => list(&query.join(" "), &filter, SearchMode::Sessions),
        Command::Search { query, filter } => list(&query.join(" "), &filter, SearchMode::All),
        Command::Show { target, grep, text } => show(&target, grep.as_deref(), text),
        Command::Resume {
            target,
            here,
            print,
        } => resume(&target, here, print),
        Command::Path { target } => {
            let session = resolve_target(&target)?;
            writeln!(io::stdout(), "{}", session.source_ref).map_err(Into::into)
        }
        Command::Convert(args) => run_convert(args),
        Command::Delete { target, yes } => delete(&target, yes),
        Command::Undo { operation, target } => undo(operation.as_deref(), target),
        Command::History { limit, text } => history(limit, text),
        Command::Compat => compat(),
        Command::ConfigSchema => {
            writeln!(io::stdout(), "{}", crate::config::config_schema_json()?).map_err(Into::into)
        }
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "sessiongator",
                &mut io::stdout(),
            );
            Ok(())
        }
    };
    // `sessiongator list | head` closes stdout early; that is not an error.
    match result {
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe) =>
        {
            Ok(())
        }
        other => other,
    }
}

fn list(query: &str, filter: &FilterArgs, mode: SearchMode) -> AppResult<()> {
    let (mut sessions, errors) = load_sessions(&filter.tools);
    for error in &errors {
        eprintln!("warning: {error}");
    }
    if sessions.is_empty() && !errors.is_empty() {
        return Err("no store could be read".into());
    }
    if let Some(root) = scope_root(filter)? {
        sessions.retain(|session| Path::new(&session.cwd).starts_with(&root));
    }
    sort_sessions(&mut sessions, filter.sort);
    let content = match mode {
        SearchMode::All => load_contents(&sessions),
        SearchMode::Sessions => HashMap::new(),
    };
    let lowercase: HashMap<String, String> = content
        .iter()
        .map(|(key, text)| (key.clone(), text.to_lowercase()))
        .collect();
    let blobs: Vec<String> = sessions.iter().map(Session::search_blob).collect();
    let hits = filter_sessions(&sessions, &blobs, query, mode, &lowercase);

    let now = now_ms();
    let mut out = io::stdout().lock();
    for index in hits.into_iter().take(filter.limit.unwrap_or(usize::MAX)) {
        let session = &sessions[index];
        let snippet = content
            .get(&session.key())
            .and_then(|text| match_snippet(text, query, 40));
        if !filter.text {
            let mut value = session_json(session);
            value["match"] = json!(snippet);
            writeln!(out, "{value}")?;
            continue;
        }
        let mut columns = vec![
            session.tool.name().to_string(),
            session.id.clone(),
            rel_time(session.updated_ms, now),
            shorten_home(&session.cwd),
            session.title.clone(),
        ];
        if mode == SearchMode::All {
            columns.push(snippet.unwrap_or_default());
        }
        writeln!(out, "{}", columns.join("\t"))?;
    }
    Ok(())
}

/// Directory that `--here` or `--dir` limits sessions to.
fn scope_root(filter: &FilterArgs) -> AppResult<Option<PathBuf>> {
    if filter.here {
        return Ok(Some(current_project_root(std::env::current_dir()?)));
    }
    match &filter.dir {
        Some(dir) => Ok(Some(std::path::absolute(dir)?)),
        None => Ok(None),
    }
}

/// Joined transcript text per session key.
fn load_contents(sessions: &[Session]) -> HashMap<String, String> {
    let targets: Vec<(Tool, String)> = sessions
        .iter()
        .map(|session| (session.tool, session.id.clone()))
        .collect();
    let content = Mutex::new(HashMap::new());
    read_transcripts(&targets, |index, text| {
        if let Some(text) = text {
            content
                .lock()
                .expect("a transcript worker panicked")
                .insert(sessions[index].key(), text);
        }
    });
    content.into_inner().expect("a transcript worker panicked")
}

fn resolve_target(target: &SessionArgs) -> AppResult<Session> {
    let (mut sessions, errors) = load_sessions(&target.tools);
    let warnings: String = errors
        .iter()
        .map(|error| format!("\nwarning: {error}"))
        .collect();
    if let Some(reference) = &target.session {
        return resolve_session(&sessions, reference)
            .cloned()
            .map_err(|message| format!("{message}{warnings}").into());
    }
    let root = current_project_root(std::env::current_dir()?);
    sort_sessions(&mut sessions, SortMode::Updated);
    sessions
        .into_iter()
        .find(|session| Path::new(&session.cwd).starts_with(&root))
        .ok_or_else(|| {
            format!(
                "no session in {}; pass a session id or search text{warnings}",
                shorten_home(&root.display().to_string())
            )
            .into()
        })
}

fn show(target: &SessionArgs, grep: Option<&str>, as_text: bool) -> AppResult<()> {
    let session = resolve_target(target)?;
    let turns = store_for(session.tool, None).transcript(&session.id)?;
    let needle = grep.map(str::to_lowercase);
    let turns: Vec<&Turn> = turns
        .iter()
        .filter(|turn| {
            needle
                .as_ref()
                .is_none_or(|needle| turn.text.to_lowercase().contains(needle))
        })
        .collect();
    let mut out = io::stdout().lock();
    if !as_text {
        let mut value = session_json(&session);
        value["turns"] = turns
            .iter()
            .map(|turn| json!({ "role": turn.role, "text": turn.text }))
            .collect();
        writeln!(out, "{value}")?;
        return Ok(());
    }
    writeln!(out, "{}", session.title)?;
    writeln!(
        out,
        "{} · {} · {}",
        session.key(),
        shorten_home(&session.cwd),
        format_utc(session.updated_ms)
    )?;
    let mut meta: Vec<String> = session.model.iter().cloned().collect();
    meta.push(format!("{} msgs", session.message_count));
    meta.extend(
        session
            .extras
            .iter()
            .map(|(key, value)| format!("{key}={value}")),
    );
    writeln!(out, "{}", meta.join(" · "))?;
    for turn in turns {
        writeln!(out, "\n[{}]\n{}", turn.role, turn.text)?;
    }
    Ok(())
}

fn resume(target: &SessionArgs, here: bool, print: bool) -> AppResult<()> {
    let session = resolve_target(target)?;
    let directory = (!here).then(|| resume_directory(&session.cwd));
    if print {
        let cwd = directory.as_ref().map(|dir| dir.display().to_string());
        writeln!(
            io::stdout(),
            "{}",
            resume_line(session.tool, &session.id, cwd.as_deref())
        )?;
        return Ok(());
    }
    let command = session.tool.resume_command(&session.id);
    let mut process = Process::new(&command[0]);
    process.args(&command[1..]);
    if let Some(directory) = &directory {
        process.current_dir(directory);
    }
    exec(process, &command[0])
}

/// The session directory, or `$HOME` when that directory no longer exists
/// (same fallback as the zsh wrapper).
fn resume_directory(cwd: &str) -> PathBuf {
    let path = PathBuf::from(cwd);
    if !cwd.is_empty() && path.is_dir() {
        return path;
    }
    let home = crate::stores::home();
    eprintln!(
        "session directory gone: {cwd}; resuming from {}",
        home.display()
    );
    home
}

#[cfg(unix)]
fn exec(mut process: Process, program: &str) -> AppResult<()> {
    use std::os::unix::process::CommandExt;
    // `exec` only returns on failure.
    let error = process.exec();
    Err(format!("cannot run {program}: {error}").into())
}

#[cfg(not(unix))]
fn exec(mut process: Process, program: &str) -> AppResult<()> {
    let status = process
        .status()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

fn delete(target: &SessionArgs, yes: bool) -> AppResult<()> {
    let session = resolve_target(target)?;
    if !yes {
        if !io::stdin().is_terminal() {
            return Err(format!("pass --yes to delete {} without a prompt", session.key()).into());
        }
        eprint!(
            "Move {} \"{}\" to the sessiongator trash? [y/N] ",
            session.key(),
            session.title
        );
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err("not deleted".into());
        }
    }
    let history = History::open();
    let footprint = store_for(session.tool, None).footprint(&session.id)?;
    let trashed = trash(
        &footprint,
        &history.trash_dir(&format!("{}-{}", session.tool.name(), session.id)),
    )?;
    let operation = history.record(Action::Delete {
        session: SessionRef {
            tool: session.tool,
            id: session.id.clone(),
            store: None,
        },
        trash: trashed.clone(),
    })?;
    println!("moved {} to {}", session.key(), trashed.dir.display());
    println!("undo: sessiongator undo {}", operation.id);
    Ok(())
}

fn undo(operation: Option<&str>, remove_target: bool) -> AppResult<()> {
    let history = History::open();
    let operation = history.find(operation)?;
    if remove_target && !matches!(operation.action, Action::Convert { .. }) {
        return Err(format!(
            "{} is not a conversion; --target does not apply",
            operation.id
        )
        .into());
    }
    println!("undo {}: {}", operation.id, operation.summary());
    for line in history.undo(&operation, remove_target)? {
        println!("  {line}");
    }
    Ok(())
}

fn history(limit: Option<usize>, as_text: bool) -> AppResult<()> {
    let operations = History::open().load()?;
    let now = now_ms();
    let mut out = io::stdout().lock();
    for operation in operations.iter().rev().take(limit.unwrap_or(usize::MAX)) {
        if !as_text {
            writeln!(out, "{}", serde_json::to_string(operation)?)?;
            continue;
        }
        let state = match operation.undone_ms {
            Some(undone) => format!("undone {}", rel_time(undone, now)),
            None => "open".to_string(),
        };
        writeln!(
            out,
            "{}\t{}\t{state}\t{}",
            operation.id,
            rel_time(operation.time_ms, now),
            operation.summary()
        )?;
    }
    Ok(())
}

fn compat() -> AppResult<()> {
    let mut out = io::stdout().lock();
    for tool in Tool::ALL {
        let installed = detect_cli_version(tool);
        let status = support(tool, installed.as_deref(), Role::Target).label();
        writeln!(
            out,
            "{}: installed {} ({status})",
            tool.name(),
            installed.as_deref().unwrap_or("none")
        )?;
        for line in describe(tool) {
            writeln!(out, "  {line}")?;
        }
    }
    Ok(())
}

fn session_json(session: &Session) -> Value {
    let extras: serde_json::Map<String, Value> = session
        .extras
        .iter()
        .map(|(key, value)| (key.clone(), json!(value)))
        .collect();
    json!({
        "tool": session.tool.name(),
        "id": session.id,
        "key": session.key(),
        "title": session.title,
        "cwd": session.cwd,
        "createdMs": session.created_ms,
        "updatedMs": session.updated_ms,
        "messageCount": session.message_count,
        "model": session.model,
        "sourceRef": session.source_ref,
        "extras": extras,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("sessiongator").chain(args.iter().copied()))
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_picker_options_and_rejects_repeated_theme() {
        let cli = parse(&["--theme", "dark", "-q", "rate"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.theme, Some(Theme::Dark));
        assert_eq!(cli.query.as_deref(), Some("rate"));
        assert!(parse(&["--theme"]).is_err());
        assert!(parse(&["--theme", "system"]).is_err());
        assert!(parse(&["--theme", "dark", "--theme", "light"]).is_err());
    }

    #[test]
    fn parses_list_filters() {
        let cli = parse(&[
            "ls", "rate", "limiter", "-t", "claude", "-t", "codex", "-p", "-n", "3",
        ])
        .unwrap();
        let Some(Command::List { query, filter }) = cli.command else {
            panic!("expected list");
        };
        assert_eq!(query, ["rate", "limiter"]);
        assert_eq!(filter.tools, [Tool::Claude, Tool::Codex]);
        assert!(filter.here);
        assert_eq!(filter.limit, Some(3));
        assert_eq!(filter.sort, SortMode::Updated);
        assert!(parse(&["list", "--here", "--dir", "/tmp"]).is_err());
        assert!(parse(&["search"]).is_err());
    }

    #[test]
    fn data_commands_default_to_json_and_take_text() {
        let Some(Command::List { filter, .. }) = parse(&["list"]).unwrap().command else {
            panic!("expected list");
        };
        assert!(!filter.text);
        let Some(Command::Search { filter, .. }) = parse(&["s", "x", "--text"]).unwrap().command
        else {
            panic!("expected search");
        };
        assert!(filter.text);
        assert!(matches!(
            parse(&["show", "--text"]).unwrap().command,
            Some(Command::Show { text: true, .. })
        ));
        assert!(matches!(
            parse(&["history"]).unwrap().command,
            Some(Command::History { text: false, .. })
        ));
        assert!(parse(&["list", "--json"]).is_err());
    }

    #[test]
    fn parses_undo_and_delete() {
        let Some(Command::Undo { operation, target }) =
            parse(&["undo", "op3", "--target"]).unwrap().command
        else {
            panic!("expected undo");
        };
        assert_eq!((operation.as_deref(), target), (Some("op3"), true));
        assert!(matches!(
            parse(&["rm", "abc", "-y"]).unwrap().command,
            Some(Command::Delete { yes: true, .. })
        ));
    }
}
