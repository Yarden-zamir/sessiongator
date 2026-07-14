use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use gator::AppResult;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};

use crate::model::{clean_title, now_ms, parse_iso_utc_ms};

const NATIVE_IMPORT_VERSIONS: &str =
    include_str!("../docs/specs/native-session-import-versions.toml");
const TARGET_SUPPORTED: &str = "target-supported";
const READ_OBSERVED: &str = "read-observed";
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImportTool {
    Claude,
    Opencode,
    Codex,
    Copilot,
}

impl ImportTool {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "claude" => Ok(Self::Claude),
            "opencode" => Ok(Self::Opencode),
            "codex" => Ok(Self::Codex),
            "copilot" => Ok(Self::Copilot),
            _ => Err(format!("unknown tool: {value}")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
        }
    }
}

#[derive(Debug)]
struct ConvertOptions {
    id: String,
    from: ImportTool,
    to: ImportTool,
    source_store: Option<PathBuf>,
    target_store: Option<PathBuf>,
    target_id: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
    dry_run: bool,
    plan_json: bool,
    report_json: bool,
    backup: bool,
    force: bool,
    allow_unsupported_version: bool,
}

#[derive(Clone, Debug)]
struct ToolVersion {
    tool: ImportTool,
    cli_version: Option<String>,
    store_version: Option<String>,
    schema_fingerprint: Option<String>,
}

#[derive(Default)]
struct ManifestToolEntry<'a> {
    tool: Option<&'a str>,
    version: Option<&'a str>,
    version_range: Option<&'a str>,
    status: Option<&'a str>,
}

#[derive(Clone, Debug)]
struct NativeSession {
    tool: ImportTool,
    id: String,
    title: String,
    cwd: String,
    created_ms: i64,
    updated_ms: i64,
    model: Option<ModelRef>,
    messages: Vec<NativeMessage>,
    metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
struct ModelRef {
    provider_id: Option<String>,
    id: String,
}

#[derive(Clone, Debug)]
struct NativeMessage {
    role: NativeRole,
    created_ms: i64,
    updated_ms: Option<i64>,
    parts: Vec<NativePart>,
    metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NativeRole {
    System,
    User,
    Assistant,
    Shell,
    Compaction,
    Unknown(String),
}

#[derive(Clone, Debug)]
enum NativePart {
    Text(String),
    Reasoning {
        text: String,
        metadata: Option<Value>,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        content: Value,
        is_error: bool,
    },
    File {
        name: Option<String>,
        mime: Option<String>,
        url: Option<String>,
        path: Option<String>,
    },
    Raw {
        kind: String,
        value: Value,
    },
}

#[derive(Debug)]
struct ConversionPlan {
    source: ToolVersion,
    target: ToolVersion,
    source_session: NativeSession,
    target_session: NativeSession,
    mapped: Vec<String>,
    dropped: Vec<String>,
    synthesized: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Debug)]
struct WriteReceipt {
    target_id: String,
    target_ref: String,
    backup: Option<PathBuf>,
    report: ConversionPlan,
}

pub fn run_convert(args: &[String]) -> AppResult<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{}", convert_usage());
        return Ok(());
    }
    let options =
        parse_convert_args(args).map_err(|message| format!("{message}\n{}", convert_usage()))?;
    let plan = build_plan(&options)?;
    if options.plan_json || options.dry_run {
        println!("{}", plan_to_json(&plan));
    }
    if options.dry_run {
        return Ok(());
    }

    let receipt = write_plan(&options, plan)?;
    if options.report_json {
        println!("{}", receipt_to_json(&receipt));
    } else {
        println!(
            "converted {}:{} -> {}:{} ({})",
            options.from.name(),
            options.id,
            options.to.name(),
            receipt.target_id,
            receipt.target_ref
        );
        if let Some(backup) = receipt.backup {
            println!("backup: {}", backup.display());
        }
    }
    Ok(())
}

fn convert_usage() -> &'static str {
    "Usage: sessiongator convert --id <id> --from <claude|opencode|codex|copilot> --to <claude|opencode|codex|copilot> [--dry-run] [--plan-json] [--report-json] [--source-store <path>] [--target-store <path>] [--target-id <id>] [--cwd <path>] [--title <title>] [--force] [--no-backup] [--allow-unsupported-version]"
}

fn parse_convert_args(args: &[String]) -> Result<ConvertOptions, String> {
    let mut id = None;
    let mut from = None;
    let mut to = None;
    let mut source_store = None;
    let mut target_store = None;
    let mut target_id = None;
    let mut cwd = None;
    let mut title = None;
    let mut dry_run = false;
    let mut plan_json = false;
    let mut report_json = false;
    let mut backup = true;
    let mut force = false;
    let mut allow_unsupported_version = false;

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--id" => id = Some(next_arg(args, &mut index, arg)?.to_string()),
            "--from" => from = Some(ImportTool::parse(next_arg(args, &mut index, arg)?)?),
            "--to" => to = Some(ImportTool::parse(next_arg(args, &mut index, arg)?)?),
            "--source-store" => {
                source_store = Some(PathBuf::from(next_arg(args, &mut index, arg)?))
            }
            "--target-store" => {
                target_store = Some(PathBuf::from(next_arg(args, &mut index, arg)?))
            }
            "--target-id" => target_id = Some(next_arg(args, &mut index, arg)?.to_string()),
            "--cwd" => cwd = Some(next_arg(args, &mut index, arg)?.to_string()),
            "--title" => title = Some(next_arg(args, &mut index, arg)?.to_string()),
            "--dry-run" => dry_run = true,
            "--plan-json" => plan_json = true,
            "--report-json" => report_json = true,
            "--backup" => backup = true,
            "--no-backup" => backup = false,
            "--force" => force = true,
            "--allow-unsupported-version" => allow_unsupported_version = true,
            "-h" | "--help" => return Err(convert_usage().to_string()),
            _ => return Err(format!("unknown convert argument: {arg}")),
        }
        index += 1;
    }

    let from = from.ok_or_else(|| "missing --from".to_string())?;
    let to = to.ok_or_else(|| "missing --to".to_string())?;
    if from == to {
        return Err("--from and --to must be different".to_string());
    }

    Ok(ConvertOptions {
        id: id.ok_or_else(|| "missing --id".to_string())?,
        from,
        to,
        source_store,
        target_store,
        target_id,
        cwd,
        title,
        dry_run,
        plan_json,
        report_json,
        backup,
        force,
        allow_unsupported_version,
    })
}

fn next_arg<'a>(args: &'a [String], index: &mut usize, name: &str) -> Result<&'a str, String> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .ok_or_else(|| format!("{name} requires a value"))
}

fn build_plan(options: &ConvertOptions) -> Result<ConversionPlan, Box<dyn std::error::Error>> {
    let source = detect_version(options.from, options.source_store.as_deref())?;
    let target = detect_version(options.to, options.target_store.as_deref())?;
    ensure_supported(&source, false, options.allow_unsupported_version)?;
    ensure_supported(&target, true, options.allow_unsupported_version)?;

    let mut source_session =
        read_session(options.from, options.source_store.as_deref(), &options.id)?;
    if let Some(cwd) = &options.cwd {
        source_session.cwd = cwd.clone();
    }
    if let Some(title) = &options.title {
        source_session.title = clean_title(title);
    }

    let (target_session, mapped, dropped, synthesized, warnings) = map_session(
        &source_session,
        options.to,
        options.target_id.clone(),
        target.cli_version.clone(),
    );
    Ok(ConversionPlan {
        source,
        target,
        source_session,
        target_session,
        mapped,
        dropped,
        synthesized,
        warnings,
    })
}

fn ensure_supported(
    version: &ToolVersion,
    target: bool,
    allow: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if allow || version_supported(version, target) {
        return Ok(());
    }
    let known = known_supported_versions(version.tool);
    Err(format!(
        "unsupported {} {} version: {:?}; known supported version is {known}",
        if target { "target" } else { "source" },
        version.tool.name(),
        version.cli_version
    )
    .into())
}

fn version_supported(version: &ToolVersion, target: bool) -> bool {
    if version
        .cli_version
        .as_deref()
        .is_some_and(|cli_version| exact_version_supported(version.tool, cli_version))
    {
        return true;
    }

    if !target {
        return version
            .store_version
            .as_deref()
            .is_some_and(|store_version| {
                observed_store_version_supported(version.tool, store_version)
            });
    }

    false
}

fn detect_version(
    tool: ImportTool,
    store: Option<&Path>,
) -> Result<ToolVersion, Box<dyn std::error::Error>> {
    let cli_version = command_version(tool);
    let (store_version, schema_fingerprint) = match tool {
        ImportTool::Claude => (None, claude_schema_fingerprint(store)?),
        ImportTool::Opencode => opencode_store_version(store)?,
        ImportTool::Codex => (None, codex_schema_fingerprint(store)?),
        ImportTool::Copilot => copilot_store_version(store)?,
    };
    Ok(ToolVersion {
        tool,
        cli_version,
        store_version,
        schema_fingerprint,
    })
}

fn command_version(tool: ImportTool) -> Option<String> {
    let output = Command::new(tool.name()).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_command_version(tool, &String::from_utf8_lossy(&output.stdout))
}

fn parse_command_version(tool: ImportTool, stdout: &str) -> Option<String> {
    let tokens = stdout.split_whitespace().collect::<Vec<_>>();
    match tool {
        ImportTool::Codex if tokens.first() == Some(&"codex-cli") => tokens.get(1),
        ImportTool::Copilot => tokens
            .iter()
            .find(|token| token.chars().next().is_some_and(|ch| ch.is_ascii_digit())),
        _ => tokens.first(),
    }
    .map(|token| token.trim_end_matches('.').to_string())
}

fn exact_version_supported(tool: ImportTool, version: &str) -> bool {
    manifest_entries().into_iter().any(|entry| {
        entry.tool == Some(tool.name())
            && entry.status == Some(TARGET_SUPPORTED)
            && entry.version == Some(version)
    })
}

fn observed_store_version_supported(tool: ImportTool, version: &str) -> bool {
    manifest_entries().into_iter().any(|entry| {
        if entry.tool != Some(tool.name()) {
            return false;
        }
        if entry.status == Some(TARGET_SUPPORTED) && entry.version == Some(version) {
            return true;
        }
        entry.status == Some(READ_OBSERVED)
            && entry
                .version_range
                .is_some_and(|range| version_in_inclusive_range(version, range))
    })
}

fn known_supported_versions(tool: ImportTool) -> String {
    let versions = exact_supported_versions(tool);
    if versions.is_empty() {
        return "none listed in native import manifest".to_string();
    }
    versions.join(", ")
}

fn default_supported_version(tool: ImportTool) -> &'static str {
    exact_supported_versions(tool)
        .into_iter()
        .last()
        .expect("native import manifest must list at least one target-supported version")
}

fn exact_supported_versions(tool: ImportTool) -> Vec<&'static str> {
    manifest_entries()
        .into_iter()
        .filter(|entry| entry.tool == Some(tool.name()) && entry.status == Some(TARGET_SUPPORTED))
        .filter_map(|entry| entry.version)
        .collect()
}

fn manifest_entries() -> Vec<ManifestToolEntry<'static>> {
    let mut entries = Vec::new();
    let mut current = None;

    for line in NATIVE_IMPORT_VERSIONS.lines() {
        let line = line.trim();
        if line == "[[tools]]" {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(ManifestToolEntry::default());
            continue;
        }

        let Some(entry) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(value) = toml_string_value(value.trim()) else {
            continue;
        };

        match key.trim() {
            "tool" => entry.tool = Some(value),
            "version" => entry.version = Some(value),
            "version_range" => entry.version_range = Some(value),
            "status" => entry.status = Some(value),
            _ => {}
        }
    }

    if let Some(entry) = current {
        entries.push(entry);
    }

    entries
}

fn toml_string_value(value: &'static str) -> Option<&'static str> {
    value
        .strip_prefix('"')?
        .split_once('"')
        .map(|(value, _)| value)
}

fn version_in_inclusive_range(version: &str, range: &str) -> bool {
    let Some((start, end)) = range.split_once("..=") else {
        return false;
    };
    compare_dotted_versions(version, start).is_some_and(|ordering| !ordering.is_lt())
        && compare_dotted_versions(version, end).is_some_and(|ordering| !ordering.is_gt())
}

fn compare_dotted_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let a = parse_dotted_version(a)?;
    let b = parse_dotted_version(b)?;
    Some(a.cmp(&b))
}

fn parse_dotted_version(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|part| part.parse().ok()).collect()
}

fn read_session(
    tool: ImportTool,
    store: Option<&Path>,
    id: &str,
) -> Result<NativeSession, Box<dyn std::error::Error>> {
    match tool {
        ImportTool::Claude => read_claude_session(store, id),
        ImportTool::Opencode => read_opencode_session(store, id),
        ImportTool::Codex => read_codex_session(store, id),
        ImportTool::Copilot => read_copilot_session(store, id),
    }
}

fn claude_root(store: Option<&Path>) -> PathBuf {
    store.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".claude"))
    })
}

fn claude_projects_dir(store: Option<&Path>) -> PathBuf {
    claude_root(store).join("projects")
}

fn claude_schema_fingerprint(
    store: Option<&Path>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let projects = claude_projects_dir(store);
    if !projects.exists() {
        return Ok(None);
    }
    let mut jsonl = 0usize;
    let mut sidecars = 0usize;
    for project in fs::read_dir(projects)? {
        let Ok(project) = project else { continue };
        let path = project.path();
        if !path.is_dir() {
            continue;
        }
        for entry in fs::read_dir(path)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                jsonl += 1;
            } else if path.is_dir() {
                sidecars += 1;
            }
        }
    }
    Ok(Some(format!("projects-jsonl:{jsonl};sidecars:{sidecars}")))
}

fn read_claude_session(
    store: Option<&Path>,
    id: &str,
) -> Result<NativeSession, Box<dyn std::error::Error>> {
    let path = find_claude_session_file(store, id)
        .ok_or_else(|| format!("claude session {id} not found"))?;
    let file = fs::File::open(&path)?;
    let mut title = None;
    let mut cwd = None;
    let mut model = None;
    let mut created_ms = None;
    let mut updated_ms = None;
    let mut messages = Vec::new();
    let mut metadata = BTreeMap::new();
    metadata.insert("source_ref".to_string(), json!(path.display().to_string()));

    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = non_empty_string(event.get("cwd"));
        }
        if let Some(branch) = non_empty_string(event.get("gitBranch")) {
            metadata
                .entry("gitBranch".to_string())
                .or_insert(json!(branch));
        }
        if let Some(version) = non_empty_string(event.get("version")) {
            metadata
                .entry("claude_version".to_string())
                .or_insert(json!(version));
        }
        if event.get("type").and_then(Value::as_str) == Some("ai-title") {
            title = non_empty_string(event.get("aiTitle")).or(title);
            continue;
        }
        let timestamp = event
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_utc_ms)
            .unwrap_or_else(now_ms);
        if created_ms.is_none() {
            created_ms = Some(timestamp);
        }
        updated_ms = Some(timestamp);
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type != "user" && event_type != "assistant" && event_type != "system" {
            continue;
        }
        let role = match event_type {
            "user" => NativeRole::User,
            "assistant" => NativeRole::Assistant,
            "system" => NativeRole::System,
            other => NativeRole::Unknown(other.to_string()),
        };
        let message = event.get("message");
        let parts = claude_message_parts(message);
        if role == NativeRole::Assistant {
            if let Some(value) = message
                .and_then(|message| message.get("model"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                model = Some(ModelRef {
                    provider_id: Some("anthropic".to_string()),
                    id: value.to_string(),
                });
            }
        }
        if title.is_none() && role == NativeRole::User {
            title = first_text_part(&parts)
                .map(|value| clean_title(&value.chars().take(120).collect::<String>()));
        }
        let mut message_metadata = BTreeMap::new();
        if let Some(uuid) = non_empty_string(event.get("uuid")) {
            message_metadata.insert("claude_uuid".to_string(), json!(uuid));
        }
        if let Some(parent) = non_empty_string(event.get("parentUuid")) {
            message_metadata.insert("claude_parent_uuid".to_string(), json!(parent));
        }
        messages.push(NativeMessage {
            role,
            created_ms: timestamp,
            updated_ms: None,
            parts,
            metadata: message_metadata,
        });
    }

    let updated = updated_ms.or(created_ms).unwrap_or_else(now_ms);
    Ok(NativeSession {
        tool: ImportTool::Claude,
        id: id.to_string(),
        title: title.unwrap_or_else(|| "(imported session)".to_string()),
        cwd: cwd.unwrap_or_default(),
        created_ms: created_ms.unwrap_or(updated),
        updated_ms: updated,
        model,
        messages,
        metadata,
    })
}

fn find_claude_session_file(store: Option<&Path>, id: &str) -> Option<PathBuf> {
    for project in fs::read_dir(claude_projects_dir(store)).ok()?.flatten() {
        let path = project.path();
        if path.is_dir() {
            let candidate = path.join(format!("{id}.jsonl"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn claude_message_parts(message: Option<&Value>) -> Vec<NativePart> {
    let Some(message) = message else {
        return Vec::new();
    };
    if let Some(text) = message.as_str().filter(|text| !text.is_empty()) {
        return vec![NativePart::Text(text.to_string())];
    }
    let Some(content) = message.get("content") else {
        return Vec::new();
    };
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        return vec![NativePart::Text(text.to_string())];
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(claude_content_block)
        .collect()
}

fn claude_content_block(block: &Value) -> Option<NativePart> {
    match block.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => non_empty_string(block.get("text")).map(NativePart::Text),
        "thinking" => non_empty_string(block.get("thinking")).map(|text| NativePart::Reasoning {
            text,
            metadata: block
                .get("signature")
                .cloned()
                .map(|signature| json!({ "signature": signature })),
        }),
        "tool_use" => Some(NativePart::ToolCall {
            id: non_empty_string(block.get("id")).unwrap_or_else(|| generated_id("tool")),
            name: non_empty_string(block.get("name")).unwrap_or_else(|| "unknown".to_string()),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => Some(NativePart::ToolResult {
            id: non_empty_string(block.get("tool_use_id")).unwrap_or_else(|| generated_id("tool")),
            content: block.get("content").cloned().unwrap_or(Value::Null),
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "file" => Some(NativePart::File {
            name: non_empty_string(block.get("filename"))
                .or_else(|| non_empty_string(block.get("name"))),
            mime: non_empty_string(block.get("mime"))
                .or_else(|| non_empty_string(block.get("mediaType"))),
            url: non_empty_string(block.get("url")),
            path: non_empty_string(block.get("path")),
        }),
        "" => None,
        other => Some(NativePart::Raw {
            kind: other.to_string(),
            value: block.clone(),
        }),
    }
}

fn opencode_db_path(store: Option<&Path>) -> PathBuf {
    store.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::var("OPENCODE_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| home_dir().join(".local").join("share"))
                    .join("opencode")
                    .join("opencode.db")
            })
    })
}

fn opencode_store_version(
    store: Option<&Path>,
) -> Result<(Option<String>, Option<String>), Box<dyn std::error::Error>> {
    let path = opencode_db_path(store);
    if !path.is_file() {
        return Ok((None, None));
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let store_version = connection
        .query_row(
            "SELECT version FROM session WHERE version IS NOT NULL AND version != '' ORDER BY time_updated DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let mut statement = connection.prepare(
        "SELECT m.name, p.name, p.type, p.[notnull]
         FROM sqlite_master m JOIN pragma_table_info(m.name) p
         WHERE m.type = 'table'
         ORDER BY m.name, p.cid",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(format!(
            "{}:{}:{}:{}",
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?
        ))
    })?;
    let fingerprint = rows.collect::<Result<Vec<_>, _>>()?.join("|");
    Ok((store_version, Some(short_hash(&fingerprint))))
}

fn read_opencode_session(
    store: Option<&Path>,
    id: &str,
) -> Result<NativeSession, Box<dyn std::error::Error>> {
    let path = opencode_db_path(store);
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (title, cwd, created_ms, updated_ms, model, version): (
        String,
        String,
        i64,
        i64,
        Option<String>,
        Option<String>,
    ) = connection.query_row(
        "SELECT title, directory, time_created, time_updated, model, version FROM session WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    )?;
    let mut metadata = BTreeMap::new();
    metadata.insert("source_ref".to_string(), json!(path.display().to_string()));
    if let Some(version) = version {
        metadata.insert("opencode_session_version".to_string(), json!(version));
    }
    Ok(NativeSession {
        tool: ImportTool::Opencode,
        id: id.to_string(),
        title,
        cwd,
        created_ms,
        updated_ms,
        model: model.as_deref().and_then(parse_opencode_model),
        messages: read_opencode_session_messages(&connection, id)?,
        metadata,
    })
}

fn read_opencode_session_messages(
    connection: &Connection,
    id: &str,
) -> Result<Vec<NativeMessage>, Box<dyn std::error::Error>> {
    if table_exists(connection, "session_message")? {
        let mut statement = connection.prepare(
            "SELECT type, time_created, time_updated, data FROM session_message WHERE session_id = ?1 ORDER BY seq, time_created, id",
        )?;
        let rows = statement.query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let (kind, created, updated, raw) = row?;
            let Ok(data) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let message = opencode_session_message_to_native(&kind, created, updated, &data);
            if !message.parts.is_empty()
                || matches!(message.role, NativeRole::System | NativeRole::Compaction)
            {
                messages.push(message);
            }
        }
        if !messages.is_empty() {
            return Ok(messages);
        }
    }
    read_opencode_legacy_messages(connection, id)
}

fn opencode_session_message_to_native(
    kind: &str,
    created: i64,
    updated: i64,
    data: &Value,
) -> NativeMessage {
    let role = match kind {
        "user" => NativeRole::User,
        "assistant" => NativeRole::Assistant,
        "system" | "synthetic" => NativeRole::System,
        "shell" => NativeRole::Shell,
        "compaction" => NativeRole::Compaction,
        other => NativeRole::Unknown(other.to_string()),
    };
    let parts = data
        .get("sessiongatorParts")
        .and_then(native_parts_from_json)
        .unwrap_or_else(|| match kind {
            "user" => non_empty_string(data.get("text"))
                .map(NativePart::Text)
                .into_iter()
                .chain(opencode_files(data.get("files")))
                .collect(),
            "assistant" => data
                .get("content")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(opencode_assistant_content)
                        .collect()
                })
                .unwrap_or_default(),
            "system" | "synthetic" | "compaction" => non_empty_string(data.get("text"))
                .or_else(|| non_empty_string(data.get("summary")))
                .map(NativePart::Text)
                .into_iter()
                .collect(),
            "shell" => vec![NativePart::Text(format!(
                "$ {}\n{}",
                non_empty_string(data.get("command")).unwrap_or_default(),
                non_empty_string(data.get("output")).unwrap_or_default()
            ))],
            _ => Vec::new(),
        });
    NativeMessage {
        role,
        created_ms: created,
        updated_ms: Some(updated),
        parts,
        metadata: BTreeMap::new(),
    }
}

fn opencode_assistant_content(value: &Value) -> Option<NativePart> {
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => non_empty_string(value.get("text")).map(NativePart::Text),
        "reasoning" => non_empty_string(value.get("text")).map(|text| NativePart::Reasoning {
            text,
            metadata: value.get("providerMetadata").cloned(),
        }),
        "tool" => Some(NativePart::ToolCall {
            id: non_empty_string(value.get("id")).unwrap_or_else(|| generated_id("tool")),
            name: non_empty_string(value.get("name")).unwrap_or_else(|| "unknown".to_string()),
            input: value
                .get("state")
                .and_then(|state| state.get("input"))
                .cloned()
                .unwrap_or(Value::Null),
        }),
        other if !other.is_empty() => Some(NativePart::Raw {
            kind: other.to_string(),
            value: value.clone(),
        }),
        _ => None,
    }
}

fn opencode_files(value: Option<&Value>) -> impl Iterator<Item = NativePart> + '_ {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|file| NativePart::File {
            name: non_empty_string(file.get("filename")),
            mime: non_empty_string(file.get("mediaType")),
            url: non_empty_string(file.get("url")),
            path: None,
        })
}

fn read_opencode_legacy_messages(
    connection: &Connection,
    id: &str,
) -> Result<Vec<NativeMessage>, Box<dyn std::error::Error>> {
    let mut statement = connection.prepare(
        "SELECT m.id, m.time_created, m.time_updated, m.data, p.data
         FROM message m LEFT JOIN part p ON p.message_id = m.id
         WHERE m.session_id = ?1
         ORDER BY m.time_created, m.id, p.time_created, p.id",
    )?;
    let rows = statement.query_map([id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut messages: Vec<NativeMessage> = Vec::new();
    let mut current_id = String::new();
    for row in rows {
        let (message_id, created, updated, message_raw, part_raw) = row?;
        if current_id != message_id {
            current_id = message_id.clone();
            let data: Value = serde_json::from_str(&message_raw).unwrap_or(Value::Null);
            let role = match data.get("role").and_then(Value::as_str).unwrap_or("") {
                "user" => NativeRole::User,
                "assistant" => NativeRole::Assistant,
                other => NativeRole::Unknown(other.to_string()),
            };
            messages.push(NativeMessage {
                role,
                created_ms: created,
                updated_ms: Some(updated),
                parts: Vec::new(),
                metadata: BTreeMap::new(),
            });
        }
        if let Some(part_raw) = part_raw {
            if let Ok(part) = serde_json::from_str::<Value>(&part_raw) {
                if let Some(native) = opencode_legacy_part(&part) {
                    if let Some(message) = messages.last_mut() {
                        message.parts.push(native);
                    }
                }
            }
        }
    }
    Ok(messages)
}

fn opencode_legacy_part(part: &Value) -> Option<NativePart> {
    match part.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => non_empty_string(part.get("text")).map(NativePart::Text),
        "reasoning" => non_empty_string(part.get("text")).map(|text| NativePart::Reasoning {
            text,
            metadata: part.get("metadata").cloned(),
        }),
        "tool" => Some(NativePart::ToolCall {
            id: non_empty_string(part.get("callID")).unwrap_or_else(|| generated_id("tool")),
            name: non_empty_string(part.get("tool")).unwrap_or_else(|| "unknown".to_string()),
            input: part
                .get("state")
                .and_then(|state| state.get("input"))
                .cloned()
                .unwrap_or(Value::Null),
        }),
        "file" => Some(NativePart::File {
            name: non_empty_string(part.get("filename")),
            mime: non_empty_string(part.get("mime")),
            url: non_empty_string(part.get("url")),
            path: None,
        }),
        _ => None,
    }
}

fn parse_opencode_model(raw: &str) -> Option<ModelRef> {
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value
            .get("id")
            .or_else(|| value.get("modelID"))
            .and_then(Value::as_str)
            .map(|id| ModelRef {
                provider_id: value
                    .get("providerID")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                id: id.to_string(),
            }),
        Err(_) => Some(ModelRef {
            provider_id: None,
            id: raw.to_string(),
        }),
    }
}

fn codex_root(store: Option<&Path>) -> PathBuf {
    store.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".codex"))
    })
}

fn codex_source_roots(store: Option<&Path>) -> Vec<PathBuf> {
    if let Some(store) = store {
        return vec![store.to_path_buf()];
    }
    let mut roots = Vec::new();
    if let Ok(root) = std::env::var("CODEX_HOME") {
        roots.push(PathBuf::from(root));
    }
    roots.push(home_dir().join(".codex"));
    roots.sort();
    roots.dedup();
    roots
}

fn codex_schema_fingerprint(
    store: Option<&Path>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut jsonl = 0usize;
    let mut has_index = false;
    let mut has_state = false;
    for root in codex_source_roots(store) {
        count_jsonl_files(&root.join("sessions"), &mut jsonl)?;
        count_jsonl_files(&root.join("archived_sessions"), &mut jsonl)?;
        has_index |= root.join("session_index.jsonl").is_file();
        has_state |= root.join("state_5.sqlite").is_file();
    }
    Ok(Some(format!(
        "codex:sessions-jsonl:{jsonl};index:{has_index};state:{has_state}"
    )))
}

fn count_jsonl_files(path: &Path, count: &mut usize) -> Result<(), Box<dyn std::error::Error>> {
    if !path.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            count_jsonl_files(&path, count)?;
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            *count += 1;
        }
    }
    Ok(())
}

fn read_codex_session(
    store: Option<&Path>,
    id: &str,
) -> Result<NativeSession, Box<dyn std::error::Error>> {
    let path = find_codex_session_file(store, id)
        .ok_or_else(|| format!("codex session {id} not found"))?;
    let file = fs::File::open(&path)?;
    let mut title = None;
    let mut cwd = None;
    let mut created_ms = None;
    let mut updated_ms = None;
    let mut model = None;
    let mut response_messages = Vec::new();
    let mut event_messages = Vec::new();
    let mut auxiliary_messages = Vec::new();
    let mut response_first_user = None;
    let mut event_first_user = None;
    let mut metadata = BTreeMap::new();
    metadata.insert("source_ref".to_string(), json!(path.display().to_string()));

    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(meta) = codex_meta_value(&event) {
            title = non_empty_string(meta.get("title")).or(title);
            cwd = non_empty_string(meta.get("cwd")).or(cwd);
            if let Some(version) = non_empty_string(meta.get("cli_version")) {
                metadata.insert("codex_cli_version".to_string(), json!(version));
            }
            if let Some(provider) = non_empty_string(meta.get("model_provider")) {
                metadata.insert("codex_model_provider".to_string(), json!(provider));
            }
            let timestamp = non_empty_string(meta.get("timestamp"))
                .and_then(|value| parse_iso_utc_ms(&value))
                .unwrap_or_else(now_ms);
            created_ms = Some(created_ms.unwrap_or(timestamp));
            updated_ms = Some(updated_ms.unwrap_or(timestamp));
            continue;
        }

        if let Some((message, source)) = codex_message_from_event(&event) {
            if created_ms.is_none() {
                created_ms = Some(message.created_ms);
            }
            updated_ms = Some(message.updated_ms.unwrap_or(message.created_ms));
            if model.is_none() {
                model = event
                    .get("payload")
                    .and_then(|payload| payload.get("model"))
                    .and_then(model_ref_from_json)
                    .or_else(|| event.get("model").and_then(model_ref_from_json));
            }
            match source {
                CodexReadSource::ResponseVisible | CodexReadSource::Sessiongator => {
                    if response_first_user.is_none() && message.role == NativeRole::User {
                        response_first_user = first_text_part(&message.parts)
                            .map(|value| clean_title(&value.chars().take(120).collect::<String>()));
                    }
                    response_messages.push(message);
                }
                CodexReadSource::ResponseAuxiliary => auxiliary_messages.push(message),
                CodexReadSource::EventMsg => {
                    if event_first_user.is_none() && message.role == NativeRole::User {
                        event_first_user = first_text_part(&message.parts)
                            .map(|value| clean_title(&value.chars().take(120).collect::<String>()));
                    }
                    event_messages.push(message);
                }
            }
        }
    }

    let updated = updated_ms.or(created_ms).unwrap_or_else(now_ms);
    let mut messages = if event_messages.is_empty() {
        response_messages
    } else {
        event_messages
    };
    messages.extend(auxiliary_messages);
    messages.sort_by_key(|message| message.created_ms);
    let fallback_title = event_first_user.or(response_first_user);
    Ok(NativeSession {
        tool: ImportTool::Codex,
        id: id.to_string(),
        title: title
            .or(fallback_title)
            .unwrap_or_else(|| "(imported session)".to_string()),
        cwd: cwd.unwrap_or_default(),
        created_ms: created_ms.unwrap_or(updated),
        updated_ms: updated,
        model,
        messages,
        metadata,
    })
}

fn find_codex_session_file(store: Option<&Path>, id: &str) -> Option<PathBuf> {
    codex_source_roots(store).into_iter().find_map(|root| {
        find_jsonl_session_file(&root.join("sessions"), id)
            .or_else(|| find_jsonl_session_file(&root.join("archived_sessions"), id))
    })
}

fn find_jsonl_session_file(root: &Path, id: &str) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_jsonl_session_file(&path, id) {
                return Some(found);
            }
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(id))
            || jsonl_file_mentions_session_id(&path, id)
        {
            return Some(path);
        }
    }
    None
}

fn jsonl_file_mentions_session_id(path: &Path, id: &str) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok).take(5) {
        if line.contains(id) {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy)]
enum CodexReadSource {
    ResponseVisible,
    ResponseAuxiliary,
    EventMsg,
    Sessiongator,
}

fn codex_meta_value(event: &Value) -> Option<&Value> {
    if event.get("type").and_then(Value::as_str) == Some("session_meta") {
        return Some(event.get("payload").unwrap_or(event));
    }
    let item = event.get("item").unwrap_or(event);
    match item.get("type").and_then(Value::as_str) {
        Some("session_meta" | "SessionMeta" | "session_meta_line") => {
            Some(item.get("meta").unwrap_or(item))
        }
        _ => None,
    }
}

fn codex_message_from_event(event: &Value) -> Option<(NativeMessage, CodexReadSource)> {
    let timestamp = event_timestamp_ms(event).unwrap_or_else(now_ms);
    if event.get("type").and_then(Value::as_str) == Some("response_item") {
        let payload = event.get("payload")?;
        return codex_response_item_message(payload, timestamp);
    }

    if event.get("type").and_then(Value::as_str) == Some("event_msg") {
        let payload = event.get("payload")?;
        let (role, text) = match payload.get("type").and_then(Value::as_str) {
            Some("user_message") => (NativeRole::User, non_empty_string(payload.get("message"))),
            Some("agent_message") => (
                NativeRole::Assistant,
                non_empty_string(payload.get("message")),
            ),
            _ => return None,
        };
        return text.map(|text| {
            (
                NativeMessage {
                    role,
                    created_ms: timestamp,
                    updated_ms: None,
                    parts: vec![NativePart::Text(text)],
                    metadata: BTreeMap::new(),
                },
                CodexReadSource::EventMsg,
            )
        });
    }

    let item = event.get("item").unwrap_or(event);
    native_message_from_event(item).map(|message| (message, CodexReadSource::Sessiongator))
}

fn codex_response_item_message(
    payload: &Value,
    timestamp: i64,
) -> Option<(NativeMessage, CodexReadSource)> {
    match payload.get("type").and_then(Value::as_str)? {
        "message" => {
            let role = match payload.get("role").and_then(Value::as_str)? {
                "user" => NativeRole::User,
                "assistant" => NativeRole::Assistant,
                _ => return None,
            };
            let parts = codex_response_parts(payload.get("content"));
            (!parts.is_empty()).then(|| {
                (
                    NativeMessage {
                        role,
                        created_ms: timestamp,
                        updated_ms: None,
                        parts,
                        metadata: BTreeMap::new(),
                    },
                    CodexReadSource::ResponseVisible,
                )
            })
        }
        "function_call" => Some((
            NativeMessage {
                role: NativeRole::Assistant,
                created_ms: timestamp,
                updated_ms: None,
                parts: vec![NativePart::ToolCall {
                    id: non_empty_string(payload.get("call_id"))
                        .unwrap_or_else(|| generated_id("tool")),
                    name: non_empty_string(payload.get("name"))
                        .unwrap_or_else(|| "unknown".to_string()),
                    input: payload
                        .get("arguments")
                        .and_then(|arguments| {
                            serde_json::from_str::<Value>(arguments.as_str().unwrap_or("{}")).ok()
                        })
                        .unwrap_or_else(|| {
                            payload.get("arguments").cloned().unwrap_or(Value::Null)
                        }),
                }],
                metadata: BTreeMap::new(),
            },
            CodexReadSource::ResponseAuxiliary,
        )),
        "function_call_output" => Some((
            NativeMessage {
                role: NativeRole::User,
                created_ms: timestamp,
                updated_ms: None,
                parts: vec![NativePart::ToolResult {
                    id: non_empty_string(payload.get("call_id"))
                        .unwrap_or_else(|| generated_id("tool")),
                    content: payload.get("output").cloned().unwrap_or(Value::Null),
                    is_error: false,
                }],
                metadata: BTreeMap::new(),
            },
            CodexReadSource::ResponseAuxiliary,
        )),
        "reasoning" => {
            let text = codex_reasoning_text(payload)?;
            Some((
                NativeMessage {
                    role: NativeRole::Assistant,
                    created_ms: timestamp,
                    updated_ms: None,
                    parts: vec![NativePart::Reasoning {
                        text,
                        metadata: payload
                            .get("summary")
                            .cloned()
                            .map(|summary| json!({ "summary": summary })),
                    }],
                    metadata: BTreeMap::new(),
                },
                CodexReadSource::ResponseAuxiliary,
            ))
        }
        "web_search_call" => Some((
            NativeMessage {
                role: NativeRole::Assistant,
                created_ms: timestamp,
                updated_ms: None,
                parts: vec![NativePart::ToolCall {
                    id: generated_id("tool"),
                    name: "web_search".to_string(),
                    input: payload.get("action").cloned().unwrap_or(Value::Null),
                }],
                metadata: BTreeMap::new(),
            },
            CodexReadSource::ResponseAuxiliary,
        )),
        other => Some((
            NativeMessage {
                role: NativeRole::Unknown(other.to_string()),
                created_ms: timestamp,
                updated_ms: None,
                parts: vec![NativePart::Raw {
                    kind: other.to_string(),
                    value: payload.clone(),
                }],
                metadata: BTreeMap::new(),
            },
            CodexReadSource::ResponseAuxiliary,
        )),
    }
}

fn codex_reasoning_text(payload: &Value) -> Option<String> {
    let summary_text = payload
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| non_empty_string(item.get("text")))
        .collect::<Vec<_>>()
        .join("\n");
    if !summary_text.is_empty() {
        return Some(summary_text);
    }
    let content_text = payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| non_empty_string(item.get("text")))
        .collect::<Vec<_>>()
        .join("\n");
    (!content_text.is_empty()).then_some(content_text)
}

fn codex_response_parts(value: Option<&Value>) -> Vec<NativePart> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                non_empty_string(part.get("text")).map(NativePart::Text)
            }
            _ => None,
        })
        .collect()
}

fn event_timestamp_ms(event: &Value) -> Option<i64> {
    event.get("created_ms").and_then(Value::as_i64).or_else(|| {
        non_empty_string(event.get("timestamp")).and_then(|value| parse_iso_utc_ms(&value))
    })
}

fn write_codex_plan(
    options: &ConvertOptions,
    plan: ConversionPlan,
) -> Result<WriteReceipt, Box<dyn std::error::Error>> {
    let root = codex_root(options.target_store.as_deref());
    let sessions_dir = codex_session_day_dir(&root, plan.target_session.created_ms);
    fs::create_dir_all(&sessions_dir)?;
    let path = sessions_dir.join(format!(
        "rollout-{}-{}.jsonl",
        codex_filename_timestamp(plan.target_session.created_ms),
        plan.target_session.id
    ));
    if path.exists() && !options.force {
        return Err(format!("target Codex session already exists: {}", path.display()).into());
    }
    let tmp = path.with_extension("jsonl.tmp");
    let mut file = fs::File::create(&tmp)?;
    writeln!(file, "{}", codex_session_meta_json(&plan))?;
    for message in &plan.target_session.messages {
        writeln!(
            file,
            "{}",
            native_message_event_json("message", message, &plan.target_session)
        )?;
    }
    file.flush()?;
    fs::rename(&tmp, &path)?;
    append_codex_session_index(&root, &plan.target_session)?;
    Ok(WriteReceipt {
        target_id: plan.target_session.id.clone(),
        target_ref: path.display().to_string(),
        backup: None,
        report: plan,
    })
}

fn codex_session_day_dir(root: &Path, epoch_ms: i64) -> PathBuf {
    let secs = epoch_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    root.join("sessions")
        .join(format!("{year:04}"))
        .join(format!("{month:02}"))
        .join(format!("{day:02}"))
}

fn codex_filename_timestamp(epoch_ms: i64) -> String {
    let iso = iso_utc(epoch_ms);
    iso.split('.').next().unwrap_or(&iso).replace(':', "-")
}

fn codex_session_meta_json(plan: &ConversionPlan) -> Value {
    json!({
        "item": {
            "type": "session_meta",
            "meta": {
                "id": plan.target_session.id,
                "session_id": plan.target_session.id,
                "timestamp": iso_utc(plan.target_session.created_ms),
                "cwd": plan.target_session.cwd,
                "title": plan.target_session.title,
                "originator": "sessiongator",
                "cli_version": plan.target.cli_version.as_deref().unwrap_or_else(|| default_supported_version(ImportTool::Codex)),
                "model_provider": plan.target_session.model.as_ref().and_then(|model| model.provider_id.as_deref()).unwrap_or("imported"),
                "source": "Cli",
                "history_mode": "legacy"
            }
        },
        "sessiongator": provenance_json(plan),
    })
}

fn append_codex_session_index(
    root: &Path,
    session: &NativeSession,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(root)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("session_index.jsonl"))?;
    writeln!(
        file,
        "{}",
        json!({ "id": session.id, "thread_name": session.title, "updated_at": iso_utc(session.updated_ms) })
    )?;
    file.flush()?;
    Ok(())
}

fn copilot_root(store: Option<&Path>) -> PathBuf {
    store.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::var("COPILOT_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".copilot"))
    })
}

fn copilot_store_version(
    store: Option<&Path>,
) -> Result<(Option<String>, Option<String>), Box<dyn std::error::Error>> {
    let path = copilot_root(store).join("session-store.db");
    if !path.is_file() {
        return Ok((None, None));
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let store_version = if table_exists(&connection, "schema_version")? {
        connection
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
            .map(|version| version.to_string())
    } else {
        None
    };
    let fingerprint = sqlite_schema_fingerprint(&connection)?;
    Ok((store_version, Some(short_hash(&fingerprint))))
}

fn sqlite_schema_fingerprint(
    connection: &Connection,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut statement = connection.prepare(
        "SELECT m.name, p.name, p.type, p.[notnull]
         FROM sqlite_master m JOIN pragma_table_info(m.name) p
         WHERE m.type IN ('table', 'virtual table')
         ORDER BY m.name, p.cid",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(format!(
            "{}:{}:{}:{}",
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?
        ))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?.join("|"))
}

fn read_copilot_session(
    store: Option<&Path>,
    id: &str,
) -> Result<NativeSession, Box<dyn std::error::Error>> {
    let root = copilot_root(store);
    let db = root.join("session-store.db");
    let mut title = None;
    let mut cwd = None;
    let mut created_ms = None;
    let mut updated_ms = None;
    let mut messages = Vec::new();
    let mut metadata = BTreeMap::new();
    metadata.insert("source_ref".to_string(), json!(root.display().to_string()));

    if db.is_file() {
        let connection = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if table_exists(&connection, "sessions")? {
            if let Some((session_cwd, summary, created, updated, repository, branch)) = connection
                .query_row(
                    "SELECT cwd, summary, created_at, updated_at, repository, branch FROM sessions WHERE id = ?1",
                    [id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .optional()?
            {
                cwd = session_cwd;
                title = summary;
                created_ms = created.and_then(|value| parse_flexible_time_ms(&value));
                updated_ms = updated.and_then(|value| parse_flexible_time_ms(&value));
                if let Some(repository) = repository {
                    metadata.insert("repository".to_string(), json!(repository));
                }
                if let Some(branch) = branch {
                    metadata.insert("branch".to_string(), json!(branch));
                }
            }
        }
        if table_exists(&connection, "turns")? {
            messages.extend(read_copilot_turns(&connection, id)?);
        }
    }

    let event_path = root.join("session-state").join(id).join("events.jsonl");
    if event_path.is_file() {
        let event_messages = read_sessiongator_events(&event_path)?;
        if !event_messages.is_empty() {
            messages = event_messages;
        }
    }

    for message in &messages {
        if created_ms.is_none() {
            created_ms = Some(message.created_ms);
        }
        updated_ms = Some(message.updated_ms.unwrap_or(message.created_ms));
        if title.is_none() && message.role == NativeRole::User {
            title = first_text_part(&message.parts)
                .map(|value| clean_title(&value.chars().take(120).collect::<String>()));
        }
    }

    let updated = updated_ms.or(created_ms).unwrap_or_else(now_ms);
    Ok(NativeSession {
        tool: ImportTool::Copilot,
        id: id.to_string(),
        title: title.unwrap_or_else(|| "(imported session)".to_string()),
        cwd: cwd.unwrap_or_default(),
        created_ms: created_ms.unwrap_or(updated),
        updated_ms: updated,
        model: None,
        messages,
        metadata,
    })
}

fn read_copilot_turns(
    connection: &Connection,
    id: &str,
) -> Result<Vec<NativeMessage>, Box<dyn std::error::Error>> {
    let mut statement = connection.prepare(
        "SELECT turn_index, user_message, assistant_response, timestamp FROM turns WHERE session_id = ?1 ORDER BY turn_index",
    )?;
    let rows = statement.query_map([id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (index, user, assistant, timestamp) = row?;
        let created = timestamp
            .and_then(|value| parse_flexible_time_ms(&value))
            .unwrap_or_else(now_ms);
        if let Some(user) = user.filter(|value| !value.is_empty()) {
            messages.push(NativeMessage {
                role: NativeRole::User,
                created_ms: created + index * 2,
                updated_ms: None,
                parts: vec![NativePart::Text(user)],
                metadata: BTreeMap::new(),
            });
        }
        if let Some(assistant) = assistant.filter(|value| !value.is_empty()) {
            messages.push(NativeMessage {
                role: NativeRole::Assistant,
                created_ms: created + index * 2 + 1,
                updated_ms: None,
                parts: vec![NativePart::Text(assistant)],
                metadata: BTreeMap::new(),
            });
        }
    }
    Ok(messages)
}

fn write_copilot_plan(
    options: &ConvertOptions,
    plan: ConversionPlan,
) -> Result<WriteReceipt, Box<dyn std::error::Error>> {
    let root = copilot_root(options.target_store.as_deref());
    fs::create_dir_all(&root)?;
    let session_root = root.join("session-state").join(&plan.target_session.id);
    if session_root.exists() && !options.force {
        return Err(format!(
            "target Copilot session already exists: {}",
            session_root.display()
        )
        .into());
    }
    fs::create_dir_all(&session_root)?;
    write_sessiongator_events(
        &session_root.join("events.jsonl"),
        "copilot",
        &plan.target_session,
    )?;
    write_copilot_workspace(&session_root.join("workspace.yaml"), &plan.target_session)?;
    write_copilot_session_db(&session_root.join("session.db"))?;
    write_copilot_store(&root.join("session-store.db"), options, &plan)?;
    Ok(WriteReceipt {
        target_id: plan.target_session.id.clone(),
        target_ref: session_root.display().to_string(),
        backup: None,
        report: plan,
    })
}

fn write_copilot_workspace(
    path: &Path,
    session: &NativeSession,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = fs::File::create(path)?;
    writeln!(file, "cwd: {}", session.cwd)?;
    writeln!(file, "summary: {}", session.title)?;
    file.flush()?;
    Ok(())
}

fn write_copilot_session_db(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS todos (id TEXT PRIMARY KEY, title TEXT NOT NULL, description TEXT, status TEXT DEFAULT 'pending' CHECK(status IN ('pending', 'in_progress', 'done', 'blocked')), created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));
        CREATE TABLE IF NOT EXISTS todo_deps (todo_id TEXT NOT NULL, depends_on TEXT NOT NULL, PRIMARY KEY (todo_id, depends_on), FOREIGN KEY (todo_id) REFERENCES todos(id), FOREIGN KEY (depends_on) REFERENCES todos(id));
        CREATE TABLE IF NOT EXISTS inbox_entries (id TEXT PRIMARY KEY, recipient_session_id TEXT NOT NULL, sender_id TEXT NOT NULL, sender_name TEXT NOT NULL, sender_type TEXT NOT NULL, interaction_id TEXT NOT NULL, sequence INTEGER NOT NULL DEFAULT 0, summary TEXT NOT NULL, content TEXT NOT NULL, unread INTEGER NOT NULL DEFAULT 1, sent_at INTEGER NOT NULL, read_at INTEGER, notified_at INTEGER);
        "#,
    )?;
    Ok(())
}

fn write_copilot_store(
    path: &Path,
    options: &ConvertOptions,
    plan: &ConversionPlan,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::open(path)?;
    create_copilot_schema_if_missing(&connection)?;
    connection.execute_batch("BEGIN IMMEDIATE;")?;
    let result = write_copilot_store_transaction(&connection, options, plan);
    match result {
        Ok(()) => connection.execute_batch("COMMIT;")?,
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK;");
            return Err(error);
        }
    }
    Ok(())
}

fn create_copilot_schema_if_missing(
    connection: &Connection,
) -> Result<(), Box<dyn std::error::Error>> {
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT, branch TEXT, summary TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));
        CREATE TABLE IF NOT EXISTS turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT, timestamp TEXT DEFAULT (datetime('now')), UNIQUE(session_id, turn_index));
        CREATE TABLE IF NOT EXISTS checkpoints (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), checkpoint_number INTEGER NOT NULL, title TEXT, overview TEXT, history TEXT, work_done TEXT, technical_details TEXT, important_files TEXT, next_steps TEXT, created_at TEXT DEFAULT (datetime('now')), UNIQUE(session_id, checkpoint_number));
        CREATE TABLE IF NOT EXISTS session_files (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), file_path TEXT NOT NULL, tool_name TEXT, turn_index INTEGER, first_seen_at TEXT DEFAULT (datetime('now')), UNIQUE(session_id, file_path));
        CREATE TABLE IF NOT EXISTS session_refs (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), ref_type TEXT NOT NULL, ref_value TEXT NOT NULL, turn_index INTEGER, created_at TEXT DEFAULT (datetime('now')), UNIQUE(session_id, ref_type, ref_value));
        "#,
    )?;
    let count: i64 =
        connection.query_row("SELECT count(*) FROM schema_version", [], |row| row.get(0))?;
    if count == 0 {
        connection.execute("INSERT INTO schema_version (version) VALUES (1)", [])?;
    }
    Ok(())
}

fn write_copilot_store_transaction(
    connection: &Connection,
    options: &ConvertOptions,
    plan: &ConversionPlan,
) -> Result<(), Box<dyn std::error::Error>> {
    let existing: Option<String> = connection
        .query_row(
            "SELECT id FROM sessions WHERE id = ?1",
            [plan.target_session.id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        if !options.force {
            return Err(format!(
                "target Copilot session already exists: {}",
                plan.target_session.id
            )
            .into());
        }
        connection.execute(
            "DELETE FROM turns WHERE session_id = ?1",
            [plan.target_session.id.as_str()],
        )?;
        connection.execute(
            "DELETE FROM sessions WHERE id = ?1",
            [plan.target_session.id.as_str()],
        )?;
    }
    connection.execute(
        "INSERT INTO sessions (id, cwd, repository, host_type, branch, summary, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            plan.target_session.id,
            plan.target_session.cwd,
            plan.target_session.cwd,
            "local",
            plan.target_session.metadata.get("gitBranch").and_then(Value::as_str),
            plan.target_session.title,
            iso_utc(plan.target_session.created_ms),
            iso_utc(plan.target_session.updated_ms),
        ],
    )?;
    for (turn, chunk) in plan.target_session.messages.chunks(2).enumerate() {
        let user = chunk
            .iter()
            .find(|message| message.role == NativeRole::User)
            .map(|message| parts_text(&message.parts));
        let assistant = chunk
            .iter()
            .find(|message| message.role == NativeRole::Assistant)
            .map(|message| parts_text(&message.parts));
        connection.execute(
            "INSERT INTO turns (session_id, turn_index, user_message, assistant_response, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![plan.target_session.id, turn as i64, user, assistant, iso_utc(chunk[0].created_ms)],
        )?;
    }
    Ok(())
}

fn write_sessiongator_events(
    path: &Path,
    tool: &str,
    session: &NativeSession,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = fs::File::create(path)?;
    writeln!(
        file,
        "{}",
        json!({
            "type": "session_meta",
            "tool": tool,
            "id": session.id,
            "title": session.title,
            "cwd": session.cwd,
            "created_ms": session.created_ms,
            "updated_ms": session.updated_ms,
            "model": session.model.as_ref().map(model_json),
        })
    )?;
    for message in &session.messages {
        writeln!(
            file,
            "{}",
            native_message_event_json("message", message, session)
        )?;
    }
    file.flush()?;
    Ok(())
}

fn read_sessiongator_events(path: &Path) -> Result<Vec<NativeMessage>, Box<dyn std::error::Error>> {
    let file = fs::File::open(path)?;
    let mut messages = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(message) = native_message_from_event(&event) {
            messages.push(message);
        }
    }
    Ok(messages)
}

fn native_message_event_json(
    kind: &str,
    message: &NativeMessage,
    session: &NativeSession,
) -> Value {
    json!({
        "type": kind,
        "role": native_role_name(&message.role),
        "created_ms": message.created_ms,
        "updated_ms": message.updated_ms,
        "parts": native_parts_to_json(&message.parts),
        "metadata": message.metadata,
        "model": session.model.as_ref().map(model_json),
    })
}

fn native_message_from_event(value: &Value) -> Option<NativeMessage> {
    if matches!(
        value.get("type").and_then(Value::as_str),
        Some("user.message" | "assistant.message")
    ) {
        let role = match value.get("type").and_then(Value::as_str)? {
            "user.message" => NativeRole::User,
            "assistant.message" => NativeRole::Assistant,
            _ => return None,
        };
        let text = value
            .get("data")
            .and_then(|data| {
                data.get("transformedContent")
                    .or_else(|| data.get("content"))
            })
            .and_then(value_text_content)?;
        return Some(NativeMessage {
            role,
            created_ms: event_timestamp_ms(value).unwrap_or_else(now_ms),
            updated_ms: None,
            parts: vec![NativePart::Text(text)],
            metadata: BTreeMap::new(),
        });
    }

    if let Some(message) = copilot_tool_or_control_message(value) {
        return Some(message);
    }

    if value.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    Some(NativeMessage {
        role: native_role_from_name(value.get("role")?.as_str()?),
        created_ms: value
            .get("created_ms")
            .and_then(Value::as_i64)
            .unwrap_or_else(now_ms),
        updated_ms: value.get("updated_ms").and_then(Value::as_i64),
        parts: value
            .get("parts")
            .and_then(native_parts_from_json)
            .unwrap_or_default(),
        metadata: value
            .get("metadata")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn copilot_tool_or_control_message(value: &Value) -> Option<NativeMessage> {
    let timestamp = event_timestamp_ms(value).unwrap_or_else(now_ms);
    let data = value.get("data").unwrap_or(value);
    match value.get("type").and_then(Value::as_str)? {
        "tool.execution_start" => Some(NativeMessage {
            role: NativeRole::Assistant,
            created_ms: timestamp,
            updated_ms: None,
            parts: vec![NativePart::ToolCall {
                id: non_empty_string(data.get("toolCallId"))
                    .unwrap_or_else(|| generated_id("tool")),
                name: non_empty_string(data.get("toolName"))
                    .or_else(|| non_empty_string(data.get("mcpToolName")))
                    .unwrap_or_else(|| "unknown".to_string()),
                input: data.get("arguments").cloned().unwrap_or(Value::Null),
            }],
            metadata: BTreeMap::new(),
        }),
        "tool.execution_complete" => Some(NativeMessage {
            role: NativeRole::User,
            created_ms: timestamp,
            updated_ms: None,
            parts: vec![NativePart::ToolResult {
                id: non_empty_string(data.get("toolCallId"))
                    .unwrap_or_else(|| generated_id("tool")),
                content: data
                    .get("result")
                    .or_else(|| data.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null),
                is_error: data.get("success").and_then(Value::as_bool) == Some(false),
            }],
            metadata: BTreeMap::new(),
        }),
        "permission.requested" => Some(NativeMessage {
            role: NativeRole::System,
            created_ms: timestamp,
            updated_ms: None,
            parts: vec![NativePart::Text(copilot_permission_text(data))],
            metadata: BTreeMap::new(),
        }),
        "assistant.turn_end" => None,
        "session.shutdown" => data.get("conversationTokens").map(|_| NativeMessage {
            role: NativeRole::Compaction,
            created_ms: timestamp,
            updated_ms: None,
            parts: vec![NativePart::Text(copilot_shutdown_text(data))],
            metadata: BTreeMap::new(),
        }),
        _ => None,
    }
}

fn copilot_permission_text(data: &Value) -> String {
    let request = data
        .get("permissionRequest")
        .or_else(|| data.get("promptRequest"))
        .map(Value::to_string)
        .unwrap_or_else(|| "{}".to_string());
    format!("permission requested {request}")
}

fn copilot_shutdown_text(data: &Value) -> String {
    let shutdown =
        non_empty_string(data.get("shutdownType")).unwrap_or_else(|| "unknown".to_string());
    let tokens = data
        .get("conversationTokens")
        .map(Value::to_string)
        .unwrap_or_else(|| "unknown".to_string());
    format!("session shutdown: {shutdown}; conversationTokens={tokens}")
}

fn value_text_content(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
        return Some(text.to_string());
    }
    let text = value
        .as_array()?
        .iter()
        .filter_map(|item| {
            item.get("text")
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn native_role_name(role: &NativeRole) -> &str {
    match role {
        NativeRole::System => "system",
        NativeRole::User => "user",
        NativeRole::Assistant => "assistant",
        NativeRole::Shell => "shell",
        NativeRole::Compaction => "compaction",
        NativeRole::Unknown(value) => value,
    }
}

fn native_role_from_name(value: &str) -> NativeRole {
    match value {
        "system" => NativeRole::System,
        "user" => NativeRole::User,
        "assistant" => NativeRole::Assistant,
        "shell" => NativeRole::Shell,
        "compaction" => NativeRole::Compaction,
        other => NativeRole::Unknown(other.to_string()),
    }
}

fn model_ref_from_json(value: &Value) -> Option<ModelRef> {
    Some(ModelRef {
        provider_id: value
            .get("providerID")
            .or_else(|| value.get("provider_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        id: value
            .get("id")
            .or_else(|| value.get("modelID"))
            .and_then(Value::as_str)?
            .to_string(),
    })
}

fn parse_flexible_time_ms(value: &str) -> Option<i64> {
    parse_iso_utc_ms(value).or_else(|| parse_iso_utc_ms(&format!("{value}Z")))
}

fn map_session(
    source: &NativeSession,
    target_tool: ImportTool,
    target_id: Option<String>,
    target_version: Option<String>,
) -> (
    NativeSession,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
) {
    let mut mapped = vec![
        "session id -> provenance".to_string(),
        "title".to_string(),
        "cwd".to_string(),
        "timestamps".to_string(),
        "messages".to_string(),
    ];
    let mut dropped = Vec::new();
    let mut synthesized = Vec::new();
    let mut warnings = Vec::new();
    let id = target_id.unwrap_or_else(|| match target_tool {
        ImportTool::Claude => generated_uuid(),
        ImportTool::Opencode => generated_id("ses"),
        ImportTool::Codex | ImportTool::Copilot => generated_uuid(),
    });
    let mut metadata = source.metadata.clone();
    metadata.insert("imported_from_tool".to_string(), json!(source.tool.name()));
    metadata.insert("imported_from_id".to_string(), json!(source.id));
    metadata.insert("imported_at_ms".to_string(), json!(now_ms()));
    if let Some(version) = target_version {
        metadata.insert("target_cli_version".to_string(), json!(version));
    }
    for message in &source.messages {
        for part in &message.parts {
            match part {
                NativePart::Raw { kind, value } => {
                    dropped.push(format!(
                        "raw part kind `{kind}` ({} bytes)",
                        value.to_string().len()
                    ));
                }
                NativePart::ToolResult { .. } if target_tool == ImportTool::Opencode => {
                    warnings.push(
                        "standalone tool results are imported as synthetic tool content"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
    }
    if source.model.is_none() {
        synthesized
            .push("target model defaults to runtime default/imported placeholder".to_string());
    } else {
        mapped.push("model".to_string());
    }
    let mut target = source.clone();
    target.tool = target_tool;
    target.id = id;
    target.metadata = metadata;
    (target, mapped, dropped, synthesized, warnings)
}

fn write_plan(
    options: &ConvertOptions,
    plan: ConversionPlan,
) -> Result<WriteReceipt, Box<dyn std::error::Error>> {
    match options.to {
        ImportTool::Claude => write_claude_plan(options, plan),
        ImportTool::Opencode => write_opencode_plan(options, plan),
        ImportTool::Codex => write_codex_plan(options, plan),
        ImportTool::Copilot => write_copilot_plan(options, plan),
    }
}

fn write_claude_plan(
    options: &ConvertOptions,
    plan: ConversionPlan,
) -> Result<WriteReceipt, Box<dyn std::error::Error>> {
    let project = claude_root(options.target_store.as_deref())
        .join("projects")
        .join(encode_claude_project_dir(&plan.target_session.cwd));
    fs::create_dir_all(&project)?;
    let path = project.join(format!("{}.jsonl", plan.target_session.id));
    if path.exists() && !options.force {
        return Err(format!("target Claude session already exists: {}", path.display()).into());
    }
    let tmp = path.with_extension("jsonl.tmp");
    let mut file = fs::File::create(&tmp)?;
    writeln!(
        file,
        "{}",
        json!({
            "type": "ai-title",
            "aiTitle": plan.target_session.title,
            "sessionId": plan.target_session.id,
            "timestamp": iso_utc(plan.target_session.created_ms),
            "version": plan
                .target
                .cli_version
                .as_deref()
                .unwrap_or_else(|| default_supported_version(ImportTool::Claude)),
            "sessiongator": provenance_json(&plan),
        })
    )?;
    let mut parent_uuid = Value::Null;
    for (index, message) in plan.target_session.messages.iter().enumerate() {
        let uuid = format!("{}-{index:04}", plan.target_session.id);
        let event = native_message_to_claude_event(&plan, message, &uuid, parent_uuid.clone());
        parent_uuid = json!(uuid);
        writeln!(file, "{event}")?;
    }
    file.flush()?;
    fs::rename(&tmp, &path)?;
    Ok(WriteReceipt {
        target_id: plan.target_session.id.clone(),
        target_ref: path.display().to_string(),
        backup: None,
        report: plan,
    })
}

fn native_message_to_claude_event(
    plan: &ConversionPlan,
    message: &NativeMessage,
    uuid: &str,
    parent_uuid: Value,
) -> Value {
    let role = match message.role {
        NativeRole::Assistant => "assistant",
        NativeRole::System => "system",
        _ => "user",
    };
    let content = if role == "assistant" {
        Value::Array(
            message
                .parts
                .iter()
                .filter_map(part_to_claude_assistant_block)
                .collect(),
        )
    } else {
        Value::Array(
            message
                .parts
                .iter()
                .filter_map(part_to_claude_user_block)
                .collect(),
        )
    };
    json!({
        "type": role,
        "sessionId": plan.target_session.id,
        "uuid": uuid,
        "parentUuid": parent_uuid,
        "cwd": plan.target_session.cwd,
        "timestamp": iso_utc(message.created_ms),
        "version": plan
            .target
            .cli_version
            .as_deref()
            .unwrap_or_else(|| default_supported_version(ImportTool::Claude)),
        "message": {
            "role": role,
            "model": plan.target_session.model.as_ref().map(|model| model.id.as_str()).unwrap_or("imported"),
            "content": content,
        },
        "sessiongator": provenance_json(plan),
    })
}

fn part_to_claude_assistant_block(part: &NativePart) -> Option<Value> {
    match part {
        NativePart::Text(text) => Some(json!({ "type": "text", "text": text })),
        NativePart::Reasoning { text, metadata } => {
            let mut value = json!({ "type": "thinking", "thinking": text });
            if let Some(signature) = metadata
                .as_ref()
                .and_then(|metadata| metadata.get("signature"))
                .cloned()
            {
                value["signature"] = signature;
            }
            Some(value)
        }
        NativePart::ToolCall { id, name, input } => Some(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        _ => None,
    }
}

fn part_to_claude_user_block(part: &NativePart) -> Option<Value> {
    match part {
        NativePart::Text(text) => Some(json!({ "type": "text", "text": text })),
        NativePart::ToolResult {
            id,
            content,
            is_error,
        } => Some(json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
            "is_error": is_error,
        })),
        NativePart::File {
            name,
            mime,
            url,
            path,
        } => Some(json!({
            "type": "file",
            "filename": name,
            "mime": mime,
            "url": url,
            "path": path,
        })),
        _ => None,
    }
}

fn write_opencode_plan(
    options: &ConvertOptions,
    plan: ConversionPlan,
) -> Result<WriteReceipt, Box<dyn std::error::Error>> {
    let path = opencode_db_path(options.target_store.as_deref());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let backup = if options.backup && path.exists() {
        let backup = path.with_extension(format!("db.sessiongator-{}.bak", now_ms()));
        fs::copy(&path, &backup)?;
        Some(backup)
    } else {
        None
    };
    let connection = Connection::open(&path)?;
    connection.execute_batch("PRAGMA foreign_keys = ON; BEGIN IMMEDIATE;")?;
    let result = write_opencode_transaction(&connection, options, &plan);
    match result {
        Ok(()) => connection.execute_batch("COMMIT;")?,
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK;");
            return Err(error);
        }
    }
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM session WHERE id = ?1",
        [plan.target_session.id.as_str()],
        |row| row.get(0),
    )?;
    if count != 1 {
        return Err("opencode write verification failed".into());
    }
    Ok(WriteReceipt {
        target_id: plan.target_session.id.clone(),
        target_ref: path.display().to_string(),
        backup,
        report: plan,
    })
}

fn write_opencode_transaction(
    connection: &Connection,
    options: &ConvertOptions,
    plan: &ConversionPlan,
) -> Result<(), Box<dyn std::error::Error>> {
    create_opencode_schema_if_missing(connection)?;
    let existing: Option<String> = connection
        .query_row(
            "SELECT id FROM session WHERE id = ?1",
            [plan.target_session.id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        if !options.force {
            return Err(format!(
                "target opencode session already exists: {}",
                plan.target_session.id
            )
            .into());
        }
        connection.execute(
            "DELETE FROM session WHERE id = ?1",
            [plan.target_session.id.as_str()],
        )?;
    }
    let project_id = ensure_opencode_project(
        connection,
        &plan.target_session.cwd,
        plan.target_session.created_ms,
    )?;
    let model_json = plan
        .target_session
        .model
        .as_ref()
        .map(|model| model_json(model).to_string());
    connection.execute(
        "INSERT INTO session
         (id, project_id, slug, directory, title, version, time_created, time_updated, agent, model, metadata,
          cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, 0, 0, 0, 0, 0)",
        params![
            plan.target_session.id,
            project_id,
            slug(&plan.target_session.title),
            plan.target_session.cwd,
            plan.target_session.title,
            plan
                .target
                .cli_version
                .as_deref()
                .unwrap_or_else(|| default_supported_version(ImportTool::Opencode)),
            plan.target_session.created_ms,
            plan.target_session.updated_ms,
            "imported",
            model_json,
            provenance_json(plan).to_string(),
        ],
    )?;
    for (seq, message) in plan.target_session.messages.iter().enumerate() {
        insert_opencode_session_message(connection, &plan.target_session, message, seq as i64 + 1)?;
        insert_opencode_legacy_message(connection, &plan.target_session, message, seq as i64 + 1)?;
    }
    Ok(())
}

fn create_opencode_schema_if_missing(
    connection: &Connection,
) -> Result<(), Box<dyn std::error::Error>> {
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT, name TEXT, icon_url TEXT, icon_color TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_initialized INTEGER, sandboxes TEXT NOT NULL DEFAULT '[]', commands TEXT, icon_url_override TEXT);
        CREATE TABLE IF NOT EXISTS project_directory (project_id TEXT NOT NULL, directory TEXT NOT NULL, type TEXT, strategy TEXT, time_created INTEGER NOT NULL, PRIMARY KEY(project_id, directory));
        CREATE TABLE IF NOT EXISTS session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT, revert TEXT, permission TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_compacting INTEGER, time_archived INTEGER, workspace_id TEXT, path TEXT, agent TEXT, model TEXT, cost REAL DEFAULT 0 NOT NULL, tokens_input INTEGER DEFAULT 0 NOT NULL, tokens_output INTEGER DEFAULT 0 NOT NULL, tokens_reasoning INTEGER DEFAULT 0 NOT NULL, tokens_cache_read INTEGER DEFAULT 0 NOT NULL, tokens_cache_write INTEGER DEFAULT 0 NOT NULL, metadata TEXT);
        CREATE TABLE IF NOT EXISTS message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, seq INTEGER NOT NULL);
        CREATE UNIQUE INDEX IF NOT EXISTS session_message_session_seq_idx ON session_message(session_id, seq);
        "#,
    )?;
    Ok(())
}

fn ensure_opencode_project(
    connection: &Connection,
    cwd: &str,
    created_ms: i64,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(id) = connection
        .query_row(
            "SELECT id FROM project WHERE worktree = ?1 LIMIT 1",
            [cwd],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    let id = generated_id("proj");
    let name = Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("imported");
    connection.execute(
        "INSERT INTO project (id, worktree, vcs, name, time_created, time_updated, sandboxes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, cwd, "git", name, created_ms, created_ms, "[]"],
    )?;
    if table_exists(connection, "project_directory")? {
        connection.execute(
            "INSERT OR IGNORE INTO project_directory (project_id, directory, type, time_created) VALUES (?1, ?2, ?3, ?4)",
            params![id, cwd, "main", created_ms],
        )?;
    }
    Ok(id)
}

fn insert_opencode_session_message(
    connection: &Connection,
    session: &NativeSession,
    message: &NativeMessage,
    seq: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = generated_id("msg");
    let kind = opencode_message_type(&message.role);
    let data = opencode_session_message_data(session, message);
    connection.execute(
        "INSERT INTO session_message (id, session_id, type, time_created, time_updated, data, seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, session.id, kind, message.created_ms, message.updated_ms.unwrap_or(message.created_ms), data.to_string(), seq],
    )?;
    Ok(())
}

fn insert_opencode_legacy_message(
    connection: &Connection,
    session: &NativeSession,
    message: &NativeMessage,
    seq: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let message_id = generated_id("msg");
    let role = if matches!(message.role, NativeRole::Assistant) {
        "assistant"
    } else {
        "user"
    };
    let message_data = json!({
        "role": role,
        "time": { "created": message.created_ms },
        "modelID": session.model.as_ref().map(|model| model.id.as_str()),
        "providerID": session.model.as_ref().and_then(|model| model.provider_id.as_deref()),
        "path": { "cwd": session.cwd, "root": session.cwd },
    });
    connection.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![message_id, session.id, message.created_ms, message.updated_ms.unwrap_or(message.created_ms), message_data.to_string()],
    )?;
    for (index, part) in message.parts.iter().enumerate() {
        let Some(data) = part_to_opencode_legacy_part(part) else {
            continue;
        };
        connection.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![generated_id("prt"), message_id, session.id, message.created_ms + index as i64 + seq, message.updated_ms.unwrap_or(message.created_ms), data.to_string()],
        )?;
    }
    Ok(())
}

fn opencode_message_type(role: &NativeRole) -> &'static str {
    match role {
        NativeRole::Assistant => "assistant",
        NativeRole::System => "system",
        NativeRole::Shell => "shell",
        NativeRole::Compaction => "compaction",
        _ => "user",
    }
}

fn opencode_session_message_data(session: &NativeSession, message: &NativeMessage) -> Value {
    match message.role {
        NativeRole::Assistant => json!({
            "agent": "imported",
            "model": session.model.as_ref().map(model_json).unwrap_or_else(|| json!({ "id": "imported", "providerID": "imported" })),
            "content": message.parts.iter().filter_map(part_to_opencode_assistant_content).collect::<Vec<_>>(),
            "sessiongatorParts": native_parts_to_json(&message.parts),
            "time": { "created": message.created_ms, "completed": message.updated_ms.unwrap_or(message.created_ms) },
            "cost": 0,
            "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
            "metadata": message.metadata,
        }),
        NativeRole::System => {
            json!({ "text": parts_text(&message.parts), "sessiongatorParts": native_parts_to_json(&message.parts), "time": { "created": message.created_ms }, "metadata": message.metadata })
        }
        NativeRole::Shell => {
            json!({ "callID": generated_id("tool"), "command": parts_text(&message.parts), "output": "", "sessiongatorParts": native_parts_to_json(&message.parts), "time": { "created": message.created_ms, "completed": message.updated_ms }, "metadata": message.metadata })
        }
        NativeRole::Compaction => {
            json!({ "reason": "manual", "summary": parts_text(&message.parts), "recent": "", "sessiongatorParts": native_parts_to_json(&message.parts), "time": { "created": message.created_ms }, "metadata": message.metadata })
        }
        _ => json!({
            "text": parts_text(&message.parts),
            "files": message.parts.iter().filter_map(part_to_opencode_file).collect::<Vec<_>>(),
            "agents": [],
            "sessiongatorParts": native_parts_to_json(&message.parts),
            "time": { "created": message.created_ms },
            "metadata": message.metadata,
        }),
    }
}

fn native_parts_to_json(parts: &[NativePart]) -> Value {
    Value::Array(parts.iter().map(native_part_to_json).collect())
}

fn native_part_to_json(part: &NativePart) -> Value {
    match part {
        NativePart::Text(text) => json!({ "type": "text", "text": text }),
        NativePart::Reasoning { text, metadata } => {
            json!({ "type": "reasoning", "text": text, "metadata": metadata })
        }
        NativePart::ToolCall { id, name, input } => {
            json!({ "type": "tool_call", "id": id, "name": name, "input": input })
        }
        NativePart::ToolResult {
            id,
            content,
            is_error,
        } => json!({ "type": "tool_result", "id": id, "content": content, "is_error": is_error }),
        NativePart::File {
            name,
            mime,
            url,
            path,
        } => json!({ "type": "file", "name": name, "mime": mime, "url": url, "path": path }),
        NativePart::Raw { kind, value } => json!({ "type": "raw", "kind": kind, "value": value }),
    }
}

fn native_parts_from_json(value: &Value) -> Option<Vec<NativePart>> {
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(native_part_from_json)
            .collect(),
    )
}

fn native_part_from_json(value: &Value) -> Option<NativePart> {
    match value.get("type")?.as_str()? {
        "text" => non_empty_string(value.get("text")).map(NativePart::Text),
        "reasoning" => non_empty_string(value.get("text")).map(|text| NativePart::Reasoning {
            text,
            metadata: value
                .get("metadata")
                .cloned()
                .filter(|value| !value.is_null()),
        }),
        "tool_call" => Some(NativePart::ToolCall {
            id: non_empty_string(value.get("id"))?,
            name: non_empty_string(value.get("name"))?,
            input: value.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => Some(NativePart::ToolResult {
            id: non_empty_string(value.get("id"))?,
            content: value.get("content").cloned().unwrap_or(Value::Null),
            is_error: value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "file" => Some(NativePart::File {
            name: non_empty_string(value.get("name")),
            mime: non_empty_string(value.get("mime")),
            url: non_empty_string(value.get("url")),
            path: non_empty_string(value.get("path")),
        }),
        "raw" => Some(NativePart::Raw {
            kind: non_empty_string(value.get("kind"))?,
            value: value.get("value").cloned().unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

fn part_to_opencode_assistant_content(part: &NativePart) -> Option<Value> {
    match part {
        NativePart::Text(text) => {
            Some(json!({ "type": "text", "id": generated_id("prt"), "text": text }))
        }
        NativePart::Reasoning { text, metadata } => Some(
            json!({ "type": "reasoning", "id": generated_id("prt"), "text": text, "providerMetadata": metadata }),
        ),
        NativePart::ToolCall { id, name, input } => Some(
            json!({ "type": "tool", "id": id, "name": name, "state": { "status": "pending", "input": input }, "time": { "created": now_ms() } }),
        ),
        NativePart::ToolResult {
            id,
            content,
            is_error,
        } => Some(json!({
            "type": "tool",
            "id": id,
            "name": "imported_tool_result",
            "state": if *is_error {
                json!({ "status": "error", "input": {}, "content": [], "structured": {}, "error": { "type": "unknown", "message": content_to_string(content) } })
            } else {
                json!({ "status": "completed", "input": {}, "content": [{ "type": "text", "text": content_to_string(content) }], "structured": {}, "result": content })
            },
            "time": { "created": now_ms(), "completed": now_ms() },
        })),
        _ => None,
    }
}

fn part_to_opencode_file(part: &NativePart) -> Option<Value> {
    match part {
        NativePart::File {
            name,
            mime,
            url,
            path,
        } => Some(json!({
            "filename": name,
            "mediaType": mime.as_deref().unwrap_or("application/octet-stream"),
            "url": url.as_ref().or(path.as_ref()),
        })),
        _ => None,
    }
}

fn part_to_opencode_legacy_part(part: &NativePart) -> Option<Value> {
    match part {
        NativePart::Text(text) => Some(json!({ "type": "text", "text": text })),
        NativePart::Reasoning { text, metadata } => {
            Some(json!({ "type": "reasoning", "text": text, "metadata": metadata }))
        }
        NativePart::ToolCall { id, name, input } => Some(
            json!({ "type": "tool", "callID": id, "tool": name, "state": { "status": "pending", "input": input } }),
        ),
        NativePart::File {
            name,
            mime,
            url,
            path,
        } => Some(
            json!({ "type": "file", "filename": name, "mime": mime, "url": url.as_ref().or(path.as_ref()) }),
        ),
        _ => None,
    }
}

fn model_json(model: &ModelRef) -> Value {
    json!({ "id": model.id, "providerID": model.provider_id.as_deref().unwrap_or("imported") })
}

fn plan_to_json(plan: &ConversionPlan) -> Value {
    json!({
        "source": version_json(&plan.source),
        "target": version_json(&plan.target),
        "sourceSession": session_summary_json(&plan.source_session),
        "targetSession": session_summary_json(&plan.target_session),
        "mapped": plan.mapped,
        "dropped": plan.dropped,
        "synthesized": plan.synthesized,
        "warnings": plan.warnings,
    })
}

fn receipt_to_json(receipt: &WriteReceipt) -> Value {
    json!({
        "targetId": receipt.target_id,
        "targetRef": receipt.target_ref,
        "backup": receipt.backup.as_ref().map(|path| path.display().to_string()),
        "report": plan_to_json(&receipt.report),
    })
}

fn version_json(version: &ToolVersion) -> Value {
    json!({
        "tool": version.tool.name(),
        "cliVersion": version.cli_version,
        "storeVersion": version.store_version,
        "schemaFingerprint": version.schema_fingerprint,
    })
}

fn session_summary_json(session: &NativeSession) -> Value {
    json!({
        "tool": session.tool.name(),
        "id": session.id,
        "title": session.title,
        "cwd": session.cwd,
        "createdMs": session.created_ms,
        "updatedMs": session.updated_ms,
        "model": session.model.as_ref().map(model_json),
        "messageCount": session.messages.len(),
        "partCounts": part_counts(&session.messages),
    })
}

fn provenance_json(plan: &ConversionPlan) -> Value {
    json!({
        "sourceTool": plan.source_session.tool.name(),
        "sourceId": plan.source_session.id,
        "sourceVersion": version_json(&plan.source),
        "targetTool": plan.target_session.tool.name(),
        "targetVersion": version_json(&plan.target),
        "convertedAtMs": now_ms(),
        "mappingProfile": "native-session-import-v1",
    })
}

fn part_counts(messages: &[NativeMessage]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for message in messages {
        for part in &message.parts {
            let key = match part {
                NativePart::Text(_) => "text",
                NativePart::Reasoning { .. } => "reasoning",
                NativePart::ToolCall { .. } => "tool_call",
                NativePart::ToolResult { .. } => "tool_result",
                NativePart::File { .. } => "file",
                NativePart::Raw { .. } => "raw",
            };
            *counts.entry(key.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

fn first_text_part(parts: &[NativePart]) -> Option<String> {
    parts.iter().find_map(|part| match part {
        NativePart::Text(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    })
}

fn parts_text(parts: &[NativePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            NativePart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(content_to_string)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string()),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, rusqlite::Error> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
}

fn encode_claude_project_dir(cwd: &str) -> String {
    let encoded = cwd.replace('/', "-");
    if encoded.is_empty() {
        "-".to_string()
    } else {
        encoded
    }
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').chars().take(80).collect::<String>()
}

fn generated_id(prefix: &str) -> String {
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}_sessiongator_{:x}_{:x}_{seq:x}",
        now_ms(),
        std::process::id()
    )
}

fn generated_uuid() -> String {
    let now = now_ms() as u128;
    let pid = u128::from(std::process::id());
    let seq = u128::from(ID_COUNTER.fetch_add(1, Ordering::Relaxed));
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (now >> 16) as u32,
        (now & 0xffff) as u16,
        (pid & 0xfff) as u16,
        ((now >> 4) & 0xfff) as u16,
        (now ^ (pid << 32) ^ seq) & 0xffffffffffff
    )
}

fn iso_utc(epoch_ms: i64) -> String {
    let secs = epoch_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        epoch_ms.rem_euclid(1000)
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = ((mp + 2) % 12 + 1) as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}

fn short_hash(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_convert_args() {
        let args = [
            "--id".to_string(),
            "abc".to_string(),
            "--from".to_string(),
            "claude".to_string(),
            "--to".to_string(),
            "opencode".to_string(),
            "--dry-run".to_string(),
        ];
        let parsed = parse_convert_args(&args).unwrap();
        assert_eq!(parsed.id, "abc");
        assert_eq!(parsed.from, ImportTool::Claude);
        assert_eq!(parsed.to, ImportTool::Opencode);
        assert!(parsed.dry_run);
    }

    #[test]
    fn supports_observed_claude_versions() {
        for version in exact_supported_versions(ImportTool::Claude) {
            let tool_version = ToolVersion {
                tool: ImportTool::Claude,
                cli_version: Some(version.to_string()),
                store_version: None,
                schema_fingerprint: Some("fixture".to_string()),
            };
            assert!(version_supported(&tool_version, true));
        }
    }

    #[test]
    fn supports_observed_opencode_store_ranges() {
        let tool_version = ToolVersion {
            tool: ImportTool::Opencode,
            cli_version: None,
            store_version: Some("1.17.10".to_string()),
            schema_fingerprint: Some("fixture".to_string()),
        };
        assert!(version_supported(&tool_version, false));
        assert!(!version_supported(&tool_version, true));
    }

    #[test]
    fn claude_parts_preserve_text_reasoning_and_tools() {
        let value: Value = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"hello"},{"type":"thinking","thinking":"hidden","signature":"sig"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a"}},{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}"#,
        )
        .unwrap();
        let parts = claude_message_parts(Some(&value));
        assert!(matches!(parts[0], NativePart::Text(_)));
        assert!(matches!(parts[1], NativePart::Reasoning { .. }));
        assert!(matches!(parts[2], NativePart::ToolCall { .. }));
        assert!(matches!(parts[3], NativePart::ToolResult { .. }));
    }

    #[test]
    fn generated_claude_project_dir_matches_existing_lossy_shape() {
        assert_eq!(
            encode_claude_project_dir("/Users/me/demo"),
            "-Users-me-demo"
        );
    }

    #[test]
    fn opencode_slug_is_stable_ascii() {
        assert_eq!(slug("Fix: Rate Limiter!"), "fix-rate-limiter");
    }

    #[test]
    fn writes_claude_session_atomically_to_isolated_store() {
        let root = temp_path("claude-write");
        let _ = fs::remove_dir_all(&root);
        let options = ConvertOptions {
            id: "source".to_string(),
            from: ImportTool::Opencode,
            to: ImportTool::Claude,
            source_store: None,
            target_store: Some(root.clone()),
            target_id: None,
            cwd: None,
            title: None,
            dry_run: false,
            plan_json: false,
            report_json: false,
            backup: true,
            force: false,
            allow_unsupported_version: true,
        };
        let plan = sample_plan(ImportTool::Claude, "11111111-2222-4333-8444-555555555555");
        let receipt = write_claude_plan(&options, plan).unwrap();
        let output = PathBuf::from(&receipt.target_ref);
        assert!(output.is_file());
        let content = fs::read_to_string(output).unwrap();
        assert!(content.contains("ai-title"));
        assert!(content.contains("hello"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn writes_opencode_session_to_isolated_db_and_reads_back() {
        let root = temp_path("opencode-write");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db = root.join("opencode.db");
        let options = ConvertOptions {
            id: "source".to_string(),
            from: ImportTool::Claude,
            to: ImportTool::Opencode,
            source_store: None,
            target_store: Some(db.clone()),
            target_id: None,
            cwd: None,
            title: None,
            dry_run: false,
            plan_json: false,
            report_json: false,
            backup: false,
            force: false,
            allow_unsupported_version: true,
        };
        let plan = sample_plan(ImportTool::Opencode, "ses_test_import");
        let receipt = write_opencode_plan(&options, plan).unwrap();
        assert_eq!(receipt.target_id, "ses_test_import");
        let readback = read_opencode_session(Some(&db), "ses_test_import").unwrap();
        assert_eq!(readback.title, "Imported Demo");
        assert_eq!(readback.cwd, "/tmp/sessiongator-demo");
        assert!(!readback.messages.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn writes_codex_session_to_isolated_store_and_reads_back() {
        let root = temp_path("codex-write");
        let _ = fs::remove_dir_all(&root);
        let options = convert_options(ImportTool::Claude, ImportTool::Codex, Some(root.clone()));
        let plan = sample_plan(ImportTool::Codex, "33333333-4444-4555-8666-777777777777");
        let receipt = write_codex_plan(&options, plan).unwrap();
        assert!(PathBuf::from(&receipt.target_ref).is_file());
        assert!(root.join("session_index.jsonl").is_file());
        let readback =
            read_codex_session(Some(&root), "33333333-4444-4555-8666-777777777777").unwrap();
        assert_eq!(readback.title, "Imported Demo");
        assert_eq!(readback.cwd, "/tmp/sessiongator-demo");
        assert_eq!(readback.messages.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn writes_copilot_session_to_isolated_store_and_reads_back() {
        let root = temp_path("copilot-write");
        let _ = fs::remove_dir_all(&root);
        let options = convert_options(ImportTool::Claude, ImportTool::Copilot, Some(root.clone()));
        let plan = sample_plan(ImportTool::Copilot, "44444444-5555-4666-8777-888888888888");
        let receipt = write_copilot_plan(&options, plan).unwrap();
        assert!(PathBuf::from(&receipt.target_ref).is_dir());
        assert!(root.join("session-store.db").is_file());
        let readback =
            read_copilot_session(Some(&root), "44444444-5555-4666-8777-888888888888").unwrap();
        assert_eq!(readback.title, "Imported Demo");
        assert_eq!(readback.cwd, "/tmp/sessiongator-demo");
        assert_eq!(readback.messages.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reads_native_codex_response_items_for_conversion() {
        let root = temp_path("codex-native-read");
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("sessions/2026/07/03");
        fs::create_dir_all(&dir).unwrap();
        let id = "99999999-aaaa-4bbb-8ccc-000000000099";
        fs::write(
            dir.join(format!("rollout-2026-07-03T10-00-01-{id}.jsonl")),
            [
                format!(r#"{{"type":"session_meta","timestamp":"2026-07-03T10:00:01.000Z","payload":{{"id":"{id}","cwd":"/tmp/native-codex","timestamp":"2026-07-03T10:00:01.000Z","cli_version":"0.143.0","model_provider":"openai"}}}}"#),
                r#"{"type":"event_msg","timestamp":"2026-07-03T10:00:01.100Z","payload":{"type":"user_message","message":"visible user"}}"#.to_string(),
                r#"{"type":"response_item","timestamp":"2026-07-03T10:00:01.100Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"native user"}]}}"#.to_string(),
                r#"{"type":"event_msg","timestamp":"2026-07-03T10:00:02.100Z","payload":{"type":"agent_message","message":"visible assistant"}}"#.to_string(),
                r#"{"type":"response_item","timestamp":"2026-07-03T10:00:02.100Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"native assistant"}]}}"#.to_string(),
                r#"{"type":"response_item","timestamp":"2026-07-03T10:00:03.100Z","payload":{"type":"function_call","call_id":"call_1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#.to_string(),
                r#"{"type":"response_item","timestamp":"2026-07-03T10:00:04.100Z","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}"#.to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        let session = read_codex_session(Some(&root), id).unwrap();
        assert_eq!(session.cwd, "/tmp/native-codex");
        assert_eq!(session.messages.len(), 4);
        assert_eq!(parts_text(&session.messages[0].parts), "visible user");
        assert_eq!(parts_text(&session.messages[1].parts), "visible assistant");
        assert!(matches!(
            session.messages[2].parts[0],
            NativePart::ToolCall { .. }
        ));
        assert!(matches!(
            session.messages[3].parts[0],
            NativePart::ToolResult { .. }
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reads_native_copilot_events_for_conversion() {
        let root = temp_path("copilot-native-read");
        let _ = fs::remove_dir_all(&root);
        let session_dir = root.join("session-state/native_copilot");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("events.jsonl"),
            r#"{"type":"user.message","timestamp":"2026-07-03T10:00:01.000Z","data":{"content":"native user","transformedContent":"native user transformed"}}
{"type":"assistant.message","timestamp":"2026-07-03T10:00:02.000Z","data":{"phase":"final_answer","content":"native assistant"}}
{"type":"tool.execution_start","timestamp":"2026-07-03T10:00:03.000Z","data":{"toolCallId":"tool_1","toolName":"shell","arguments":{"cmd":"ls"}}}
{"type":"tool.execution_complete","timestamp":"2026-07-03T10:00:04.000Z","data":{"toolCallId":"tool_1","success":true,"result":"ok"}}
{"type":"system.message","timestamp":"2026-07-03T10:00:03.000Z","data":{"role":"system","content":"hidden system"}}"#,
        )
        .unwrap();
        let session = read_copilot_session(Some(&root), "native_copilot").unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(
            parts_text(&session.messages[0].parts),
            "native user transformed"
        );
        assert_eq!(parts_text(&session.messages[1].parts), "native assistant");
        assert!(matches!(
            session.messages[2].parts[0],
            NativePart::ToolCall { .. }
        ));
        assert!(matches!(
            session.messages[3].parts[0],
            NativePart::ToolResult { .. }
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn roundtrips_codex_and_copilot_in_both_directions() {
        let root = temp_path("codex-copilot-roundtrip");
        let _ = fs::remove_dir_all(&root);
        let codex_root = root.join("codex");
        let copilot_root = root.join("copilot");

        let codex_options = convert_options(
            ImportTool::Claude,
            ImportTool::Codex,
            Some(codex_root.clone()),
        );
        write_codex_plan(
            &codex_options,
            sample_plan(ImportTool::Codex, "55555555-6666-4777-8888-999999999999"),
        )
        .unwrap();
        let codex_readback =
            read_codex_session(Some(&codex_root), "55555555-6666-4777-8888-999999999999").unwrap();
        let codex_to_copilot = plan_from_session(
            codex_readback,
            ImportTool::Copilot,
            "66666666-7777-4888-8999-aaaaaaaaaaaa",
        );
        let copilot_options = convert_options(
            ImportTool::Codex,
            ImportTool::Copilot,
            Some(copilot_root.clone()),
        );
        write_copilot_plan(&copilot_options, codex_to_copilot).unwrap();
        let copilot_readback =
            read_copilot_session(Some(&copilot_root), "66666666-7777-4888-8999-aaaaaaaaaaaa")
                .unwrap();

        let second_codex_root = root.join("codex-again");
        let copilot_to_codex = plan_from_session(
            copilot_readback,
            ImportTool::Codex,
            "77777777-8888-4999-8aaa-bbbbbbbbbbbb",
        );
        let second_codex_options = convert_options(
            ImportTool::Copilot,
            ImportTool::Codex,
            Some(second_codex_root.clone()),
        );
        write_codex_plan(&second_codex_options, copilot_to_codex).unwrap();
        let final_readback = read_codex_session(
            Some(&second_codex_root),
            "77777777-8888-4999-8aaa-bbbbbbbbbbbb",
        )
        .unwrap();
        assert_eq!(final_readback.title, "Imported Demo");
        assert_eq!(final_readback.cwd, "/tmp/sessiongator-demo");
        assert_eq!(parts_text(&final_readback.messages[0].parts), "hello");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn conversion_plan_supports_all_cross_tool_pairs() {
        let tools = [
            ImportTool::Claude,
            ImportTool::Opencode,
            ImportTool::Codex,
            ImportTool::Copilot,
        ];
        for from in tools {
            for to in tools {
                if from == to {
                    continue;
                }
                let mut source = sample_plan(from, "source_id").source_session;
                source.tool = from;
                source.id = format!("{}_source", from.name());
                let (target, mapped, dropped, _, _) = map_session(
                    &source,
                    to,
                    Some(format!("{}_target", to.name())),
                    Some(default_supported_version(to).to_string()),
                );
                assert_eq!(target.tool, to);
                assert_eq!(target.id, format!("{}_target", to.name()));
                assert!(mapped.iter().any(|item| item == "messages"));
                assert!(dropped.is_empty());
            }
        }
    }

    #[test]
    fn native_import_fixture_claude_basic_converts_to_opencode() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/native-import/claude/2.1.199/basic-text/source");
        let session =
            read_claude_session(Some(&fixture), "11111111-2222-4333-8444-555555555555").unwrap();
        assert_eq!(session.title, "Fixture native import demo");
        assert_eq!(session.messages.len(), 3);
        assert_eq!(part_counts(&session.messages).get("reasoning"), Some(&1));
        assert_eq!(part_counts(&session.messages).get("tool_call"), Some(&1));
        assert_eq!(part_counts(&session.messages).get("tool_result"), Some(&1));

        let root = temp_path("fixture-claude-to-opencode");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db = root.join("opencode.db");
        let source = ToolVersion {
            tool: ImportTool::Claude,
            cli_version: Some(default_supported_version(ImportTool::Claude).to_string()),
            store_version: None,
            schema_fingerprint: Some("fixture".to_string()),
        };
        let target = ToolVersion {
            tool: ImportTool::Opencode,
            cli_version: Some(default_supported_version(ImportTool::Opencode).to_string()),
            store_version: None,
            schema_fingerprint: Some("fixture".to_string()),
        };
        let (target_session, mapped, dropped, synthesized, warnings) = map_session(
            &session,
            ImportTool::Opencode,
            Some("ses_fixture_import".to_string()),
            Some(default_supported_version(ImportTool::Opencode).to_string()),
        );
        let plan = ConversionPlan {
            source,
            target,
            source_session: session,
            target_session,
            mapped,
            dropped,
            synthesized,
            warnings,
        };
        let options = ConvertOptions {
            id: "11111111-2222-4333-8444-555555555555".to_string(),
            from: ImportTool::Claude,
            to: ImportTool::Opencode,
            source_store: Some(fixture),
            target_store: Some(db.clone()),
            target_id: Some("ses_fixture_import".to_string()),
            cwd: None,
            title: None,
            dry_run: false,
            plan_json: false,
            report_json: false,
            backup: false,
            force: false,
            allow_unsupported_version: true,
        };
        write_opencode_plan(&options, plan).unwrap();
        let readback = read_opencode_session(Some(&db), "ses_fixture_import").unwrap();
        assert_eq!(readback.title, "Fixture native import demo");
        assert_eq!(readback.cwd, "/tmp/sessiongator-demo");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn native_import_fixture_roundtrips_claude_opencode_claude_identically() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/native-import/claude/2.1.199/basic-text/source");
        let source_id = "11111111-2222-4333-8444-555555555555";
        let source_session = read_claude_session(Some(&fixture), source_id).unwrap();

        let root = temp_path("fixture-roundtrip");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db = root.join("opencode.db");
        let claude_target = root.join("claude-target");

        let to_opencode = plan_from_session(
            source_session.clone(),
            ImportTool::Opencode,
            "ses_roundtrip_import",
        );
        let opencode_options = ConvertOptions {
            id: source_id.to_string(),
            from: ImportTool::Claude,
            to: ImportTool::Opencode,
            source_store: Some(fixture),
            target_store: Some(db.clone()),
            target_id: Some("ses_roundtrip_import".to_string()),
            cwd: None,
            title: None,
            dry_run: false,
            plan_json: false,
            report_json: false,
            backup: false,
            force: false,
            allow_unsupported_version: true,
        };
        write_opencode_plan(&opencode_options, to_opencode).unwrap();
        let opencode_readback = read_opencode_session(Some(&db), "ses_roundtrip_import").unwrap();

        let to_claude = plan_from_session(
            opencode_readback,
            ImportTool::Claude,
            "22222222-3333-4444-8555-666666666666",
        );
        let claude_options = ConvertOptions {
            id: "ses_roundtrip_import".to_string(),
            from: ImportTool::Opencode,
            to: ImportTool::Claude,
            source_store: Some(db),
            target_store: Some(claude_target.clone()),
            target_id: Some("22222222-3333-4444-8555-666666666666".to_string()),
            cwd: None,
            title: None,
            dry_run: false,
            plan_json: false,
            report_json: false,
            backup: false,
            force: false,
            allow_unsupported_version: true,
        };
        write_claude_plan(&claude_options, to_claude).unwrap();
        let roundtrip =
            read_claude_session(Some(&claude_target), "22222222-3333-4444-8555-666666666666")
                .unwrap();

        assert_eq!(
            normalized_session(&source_session),
            normalized_session(&roundtrip)
        );
        let _ = fs::remove_dir_all(root);
    }

    fn sample_plan(target_tool: ImportTool, target_id: &str) -> ConversionPlan {
        let source = ToolVersion {
            tool: ImportTool::Claude,
            cli_version: Some(default_supported_version(ImportTool::Claude).to_string()),
            store_version: None,
            schema_fingerprint: Some("fixture".to_string()),
        };
        let target = ToolVersion {
            tool: target_tool,
            cli_version: Some(
                match target_tool {
                    ImportTool::Claude => default_supported_version(ImportTool::Claude),
                    ImportTool::Opencode => default_supported_version(ImportTool::Opencode),
                    ImportTool::Codex => default_supported_version(ImportTool::Codex),
                    ImportTool::Copilot => default_supported_version(ImportTool::Copilot),
                }
                .to_string(),
            ),
            store_version: None,
            schema_fingerprint: Some("fixture".to_string()),
        };
        let source_session = NativeSession {
            tool: ImportTool::Claude,
            id: "source".to_string(),
            title: "Imported Demo".to_string(),
            cwd: "/tmp/sessiongator-demo".to_string(),
            created_ms: 1_783_000_000_000,
            updated_ms: 1_783_000_001_000,
            model: Some(ModelRef {
                provider_id: Some("anthropic".to_string()),
                id: "claude-test".to_string(),
            }),
            messages: vec![NativeMessage {
                role: NativeRole::User,
                created_ms: 1_783_000_000_000,
                updated_ms: None,
                parts: vec![NativePart::Text("hello".to_string())],
                metadata: BTreeMap::new(),
            }],
            metadata: BTreeMap::new(),
        };
        let mut target_session = source_session.clone();
        target_session.tool = target_tool;
        target_session.id = target_id.to_string();
        ConversionPlan {
            source,
            target,
            source_session,
            target_session,
            mapped: vec!["messages".to_string()],
            dropped: Vec::new(),
            synthesized: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn convert_options(
        from: ImportTool,
        to: ImportTool,
        target_store: Option<PathBuf>,
    ) -> ConvertOptions {
        ConvertOptions {
            id: "source".to_string(),
            from,
            to,
            source_store: None,
            target_store,
            target_id: None,
            cwd: None,
            title: None,
            dry_run: false,
            plan_json: false,
            report_json: false,
            backup: false,
            force: false,
            allow_unsupported_version: true,
        }
    }

    fn plan_from_session(
        source_session: NativeSession,
        target_tool: ImportTool,
        target_id: &str,
    ) -> ConversionPlan {
        let source = ToolVersion {
            tool: source_session.tool,
            cli_version: Some(
                match source_session.tool {
                    ImportTool::Claude => default_supported_version(ImportTool::Claude),
                    ImportTool::Opencode => default_supported_version(ImportTool::Opencode),
                    ImportTool::Codex => default_supported_version(ImportTool::Codex),
                    ImportTool::Copilot => default_supported_version(ImportTool::Copilot),
                }
                .to_string(),
            ),
            store_version: None,
            schema_fingerprint: Some("fixture".to_string()),
        };
        let target = ToolVersion {
            tool: target_tool,
            cli_version: Some(
                match target_tool {
                    ImportTool::Claude => default_supported_version(ImportTool::Claude),
                    ImportTool::Opencode => default_supported_version(ImportTool::Opencode),
                    ImportTool::Codex => default_supported_version(ImportTool::Codex),
                    ImportTool::Copilot => default_supported_version(ImportTool::Copilot),
                }
                .to_string(),
            ),
            store_version: None,
            schema_fingerprint: Some("fixture".to_string()),
        };
        let (target_session, mapped, dropped, synthesized, warnings) = map_session(
            &source_session,
            target_tool,
            Some(target_id.to_string()),
            target.cli_version.clone(),
        );
        ConversionPlan {
            source,
            target,
            source_session,
            target_session,
            mapped,
            dropped,
            synthesized,
            warnings,
        }
    }

    fn normalized_session(session: &NativeSession) -> Value {
        json!({
            "title": session.title,
            "cwd": session.cwd,
            "model": session.model.as_ref().map(model_json),
            "messages": session.messages.iter().map(normalized_message).collect::<Vec<_>>(),
        })
    }

    fn normalized_message(message: &NativeMessage) -> Value {
        json!({
            "role": normalized_role(&message.role),
            "parts": message.parts.iter().map(normalized_part).collect::<Vec<_>>(),
        })
    }

    fn normalized_role(role: &NativeRole) -> &str {
        match role {
            NativeRole::System => "system",
            NativeRole::User => "user",
            NativeRole::Assistant => "assistant",
            NativeRole::Shell => "shell",
            NativeRole::Compaction => "compaction",
            NativeRole::Unknown(value) => value,
        }
    }

    fn normalized_part(part: &NativePart) -> Value {
        match part {
            NativePart::Text(text) => json!({ "type": "text", "text": text }),
            NativePart::Reasoning { text, metadata } => {
                json!({ "type": "reasoning", "text": text, "metadata": metadata })
            }
            NativePart::ToolCall { id, name, input } => {
                json!({ "type": "tool_call", "id": id, "name": name, "input": input })
            }
            NativePart::ToolResult {
                id,
                content,
                is_error,
            } => json!({
                "type": "tool_result",
                "id": id,
                "content": content,
                "is_error": is_error,
            }),
            NativePart::File {
                name,
                mime,
                url,
                path,
            } => json!({
                "type": "file",
                "name": name,
                "mime": mime,
                "url": url,
                "path": path,
            }),
            NativePart::Raw { kind, value } => {
                json!({ "type": "raw", "kind": kind, "value": value })
            }
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sessiongator-native-import-{name}-{}",
            std::process::id()
        ))
    }
}
