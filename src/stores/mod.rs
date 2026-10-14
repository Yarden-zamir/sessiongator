//! One `Store` per AI tool. A store lists sessions, reads one session into
//! the native model, writes a converted session, and reports which files,
//! rows, and lines belong to a session so `trash` can remove and restore them.

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod opencode;
mod sqlite;

use std::path::{Path, PathBuf};

use gator::AppResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Session, Tool};
use crate::native::{turns, NativeSession, Turn};

pub trait Store {
    fn tool(&self) -> Tool;
    fn available(&self) -> bool;
    /// Metadata of every session in the store, without transcript bodies.
    fn list(&self) -> AppResult<Vec<Session>>;
    fn read(&self, id: &str) -> AppResult<NativeSession>;
    fn transcript(&self, id: &str) -> AppResult<Vec<Turn>> {
        Ok(turns(&self.read(id)?))
    }
    fn fingerprint(&self) -> AppResult<Fingerprint>;
    fn write(&self, request: &WriteRequest) -> AppResult<Written>;
    /// Everything that belongs to session `id`.
    fn footprint(&self, id: &str) -> AppResult<Footprint>;
    /// Append one assistant text message to session `id`. Returns the
    /// footprint of the added message, so `trash` can remove it again.
    fn append_note(&self, id: &str, note: &Note) -> AppResult<Footprint>;
}

/// Store format facts for the conversion report.
pub struct Fingerprint {
    pub store_version: Option<String>,
    pub schema: Option<String>,
}

pub struct WriteRequest<'a> {
    pub session: &'a NativeSession,
    /// Version string stamped into the written session.
    pub cli_version: &'a str,
    pub provenance: &'a Value,
    pub force: bool,
    /// Copy a live SQLite store before writing.
    pub backup: bool,
}

#[derive(Debug)]
pub struct Written {
    pub reference: String,
    pub backup: Option<PathBuf>,
}

pub struct Note<'a> {
    pub text: &'a str,
    pub cli_version: &'a str,
}

/// The parts of a store that make up one session (or one note in it).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    /// Files and directories that belong only to the session.
    pub paths: Vec<PathBuf>,
    /// SQLite rows in delete order: children before parents.
    pub rows: Vec<RowSet>,
    /// Exact lines inside shared JSONL files.
    pub lines: Vec<LineSet>,
}

/// The rows of `table` where `column` equals `value`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowSet {
    pub db: PathBuf,
    pub table: String,
    pub column: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LineSet {
    pub file: PathBuf,
    pub lines: Vec<String>,
}

/// The store of `tool` at `root`, or at its default location when `root` is
/// `None`. `root` is a config directory, or the database file for opencode.
pub fn store_for(tool: Tool, root: Option<&Path>) -> Box<dyn Store> {
    let root = root.map(Path::to_path_buf);
    match tool {
        Tool::Claude => Box::new(claude::ClaudeStore::new(
            root.unwrap_or_else(claude::default_root),
        )),
        Tool::Opencode => Box::new(opencode::OpencodeStore::new(
            root.unwrap_or_else(opencode::default_db),
        )),
        Tool::Codex => Box::new(match root {
            Some(root) => codex::CodexStore::new(vec![root]),
            None => codex::CodexStore::new(codex::default_roots()),
        }),
        Tool::Copilot => Box::new(copilot::CopilotStore::new(
            root.unwrap_or_else(copilot::default_root),
        )),
    }
}

pub fn stores_from_env() -> Vec<Box<dyn Store>> {
    Tool::ALL
        .iter()
        .map(|tool| store_for(*tool, None))
        .collect()
}

/// Sessions from every available store, filtered to `tools` when it is not
/// empty, plus one error message per store that failed to list.
pub fn load_sessions(tools: &[Tool]) -> (Vec<Session>, Vec<String>) {
    let mut sessions = Vec::new();
    let mut errors = Vec::new();
    for store in stores_from_env() {
        if !store.available() || !(tools.is_empty() || tools.contains(&store.tool())) {
            continue;
        }
        match store.list() {
            Ok(batch) => sessions.extend(batch),
            Err(error) => errors.push(format!("{}: {error}", store.tool().name())),
        }
    }
    (sessions, errors)
}

/// Read the transcript of every `(tool, id)` on several threads and call
/// `found(index, text)` with the joined transcript text, in completion order. A transcript that fails to read yields `None`.
pub fn read_transcripts(targets: &[(Tool, String)], found: impl Fn(usize, Option<String>) + Sync) {
    if targets.is_empty() {
        return;
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(8);
    let size = targets.len().div_ceil(workers);
    std::thread::scope(|scope| {
        for (chunk_index, chunk) in targets.chunks(size).enumerate() {
            let found = &found;
            scope.spawn(move || {
                let stores = stores_from_env();
                for (offset, (tool, id)) in chunk.iter().enumerate() {
                    let text = stores
                        .iter()
                        .find(|store| store.tool() == *tool)
                        .and_then(|store| store.transcript(id).ok())
                        .map(|turns| {
                            turns
                                .iter()
                                .map(|turn| turn.text.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        });
                    found(chunk_index * size + offset, text);
                }
            });
        }
    });
}

pub fn home() -> PathBuf {
    std::env::home_dir().expect("the home directory is unknown; set HOME")
}

/// `$name` when it is set and not empty.
pub(crate) fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Write `content` to `path` through a temporary file and a rename, so a
/// reader never sees a half-written file.
fn write_atomic(path: &Path, content: &str) -> AppResult<()> {
    let tmp = path.with_extension("sessiongator.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Append `lines` to the JSONL file at `path`. Adds a newline first when the
/// file does not end with one.
fn append_lines(path: &Path, lines: &[String]) -> AppResult<()> {
    use std::io::Write;
    let existing = std::fs::read(path)?;
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    if existing.last().is_some_and(|byte| *byte != b'\n') {
        writeln!(file)?;
    }
    for line in lines {
        writeln!(file, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::native::{ModelRef, NativeMessage, NativeRole};

    /// A new empty directory, unique per call.
    pub fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sessiongator-test-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn sample_session(tool: Tool, id: &str) -> NativeSession {
        NativeSession {
            tool,
            id: id.to_string(),
            title: "Imported Demo".to_string(),
            cwd: "/tmp/sessiongator-demo".to_string(),
            created_ms: 1_783_000_000_000,
            updated_ms: 1_783_000_001_000,
            model: Some(ModelRef {
                provider_id: Some("anthropic".to_string()),
                id: "claude-test".to_string(),
            }),
            messages: vec![NativeMessage::text(
                NativeRole::User,
                1_783_000_000_000,
                "hello".to_string(),
            )],
            metadata: BTreeMap::new(),
        }
    }
}
