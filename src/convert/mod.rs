//! `sessiongator convert`: read one session, check versions, map it into
//! the target tool's native store, and optionally mark or delete the source.

pub mod versions;

use std::path::PathBuf;

use gator::AppResult;
use serde_json::{json, Value};

use crate::history::{Action, History, Operation, SessionRef};
use crate::model::{clean_title, resume_line, Tool};
use crate::native::{
    generated_id, generated_uuid, model_json, part_counts, parts_text, NativePart, NativeRole,
    NativeSession,
};
use crate::search::resolve_session;
use crate::stores::{load_sessions, store_for, Footprint, Note, Store, WriteRequest};
use crate::time::now_ms;
use crate::trash::{trash, Trashed};
use versions::{detect_cli_version, newest_known, support, Role, Support};

/// Arguments for `sessiongator convert`.
#[derive(clap::Args, Debug)]
#[command(after_help = "Examples:
  sessiongator convert e4255e84 --dry-run
  sessiongator convert 'rate limiter' --to codex --mark-source
  sessiongator convert --from claude --to codex --id <session-id> --dry-run --plan-json")]
pub struct ConvertArgs {
    /// Session to convert: id, unique id prefix, `tool:id`, or search text.
    #[arg(conflicts_with = "id")]
    session: Option<String>,
    /// Exact source session id.
    #[arg(long)]
    id: Option<String>,
    /// Source tool. Default: the tool that owns the session.
    #[arg(long, value_enum)]
    from: Option<Tool>,
    /// Target tool. Default: codex for Claude sessions, else claude.
    #[arg(long, value_enum)]
    to: Option<Tool>,
    /// Source config root or database. Requires --from and --id.
    #[arg(long)]
    source_store: Option<PathBuf>,
    /// Target config root or database. Default: the target tool's normal store.
    #[arg(long)]
    target_store: Option<PathBuf>,
    /// Target session id. Default: a generated native id.
    #[arg(long)]
    target_id: Option<String>,
    /// Target working directory. Default: the source session cwd.
    #[arg(long)]
    cwd: Option<String>,
    /// Target title. Default: the source title.
    #[arg(long)]
    title: Option<String>,
    /// Append a note to the source session that names the new session and
    /// how to resume it. `sessiongator undo` removes the note.
    #[arg(long, conflicts_with = "delete_source")]
    mark_source: bool,
    /// Move the source session to the sessiongator trash after the
    /// conversion. `sessiongator undo` restores it.
    #[arg(long)]
    delete_source: bool,
    /// Read, check, and map without writing. Prints a summary.
    #[arg(long)]
    dry_run: bool,
    /// Print the conversion plan as JSON.
    #[arg(long)]
    plan_json: bool,
    /// Print the post-write report as JSON.
    #[arg(long)]
    report_json: bool,
    /// Copy the whole opencode database before writing. Undo does not need it.
    #[arg(long)]
    backup: bool,
    /// Overwrite an existing target session id.
    #[arg(long)]
    force: bool,
    /// Convert even when the target version is unsupported.
    #[arg(long)]
    allow_unsupported_version: bool,
}

/// What happens to the source session after a successful write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterWrite {
    Keep,
    Mark,
    Delete,
}

#[derive(Debug)]
pub struct ConvertOptions {
    pub id: String,
    pub from: Tool,
    pub to: Tool,
    pub source_store: Option<PathBuf>,
    pub target_store: Option<PathBuf>,
    pub target_id: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub after: AfterWrite,
    pub dry_run: bool,
    pub plan_json: bool,
    pub report_json: bool,
    pub backup: bool,
    pub force: bool,
    pub allow_unsupported_version: bool,
}

/// The version a conversion checked for one side, with store facts.
#[derive(Debug)]
pub struct VersionReport {
    pub tool: Tool,
    /// The checked version: the version that wrote the source session, or
    /// the installed target CLI.
    pub version: Option<String>,
    pub cli_version: Option<String>,
    pub store_version: Option<String>,
    pub schema: Option<String>,
    pub support: Support,
}

#[derive(Debug)]
pub struct ConversionPlan {
    pub source: VersionReport,
    pub target: VersionReport,
    pub source_session: NativeSession,
    pub target_session: NativeSession,
    pub mapped: Vec<String>,
    pub dropped: Vec<String>,
    pub synthesized: Vec<String>,
    pub warnings: Vec<String>,
}

pub struct Receipt {
    pub reference: String,
    pub backup: Option<PathBuf>,
    pub operation: Operation,
    pub plan: ConversionPlan,
}

/// Version gate failure. `run_convert` exits with code 3 for this error.
#[derive(Debug)]
pub struct UnsupportedVersion(String);

impl std::fmt::Display for UnsupportedVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UnsupportedVersion {}

pub fn run_convert(args: ConvertArgs) -> AppResult<()> {
    let options = options_from_args(args).unwrap_or_else(|message| {
        eprintln!("error: {message}");
        std::process::exit(2);
    });
    let plan = match build_plan(&options) {
        Ok(plan) => plan,
        Err(error) if error.is::<UnsupportedVersion>() => {
            eprintln!("error: {error}");
            std::process::exit(3);
        }
        Err(error) => return Err(error),
    };
    if options.plan_json {
        println!("{}", plan_json(&plan));
    } else if options.dry_run {
        print_summary(&plan, &options);
    }
    if options.dry_run {
        return Ok(());
    }
    if !options.plan_json && !options.report_json {
        for warning in &plan.warnings {
            eprintln!("warning: {warning}");
        }
    }
    let receipt = convert(&options, plan, &History::open())?;
    if options.report_json {
        println!("{}", receipt_json(&receipt));
        return Ok(());
    }
    let target = &receipt.plan.target_session;
    println!(
        "converted {}:{} -> {}:{} ({})",
        options.from.name(),
        options.id,
        options.to.name(),
        target.id,
        receipt.reference
    );
    if let Some(backup) = &receipt.backup {
        println!("backup: {}", backup.display());
    }
    if let Action::Convert {
        note, source_trash, ..
    } = &receipt.operation.action
    {
        if note.is_some() {
            println!("marked the source session with a migration note");
        }
        if let Some(trashed) = source_trash {
            println!("moved the source session to {}", trashed.dir.display());
        }
    }
    println!(
        "resume: {}",
        resume_line(options.to, &target.id, Some(&target.cwd))
    );
    println!("undo:   sessiongator undo {}", receipt.operation.id);
    Ok(())
}

fn options_from_args(args: ConvertArgs) -> Result<ConvertOptions, String> {
    if args.source_store.is_some() && (args.from.is_none() || args.id.is_none()) {
        return Err("--source-store requires --from and --id".to_string());
    }
    let (from, id) = match (args.from, args.id, args.session) {
        (Some(from), Some(id), _) => (from, id),
        (from, id, session) => {
            let reference = session.or(id).ok_or_else(|| {
                "missing session: pass an id, id prefix, tool:id, or search text".to_string()
            })?;
            let tools: Vec<Tool> = from.into_iter().collect();
            let (sessions, errors) = load_sessions(&tools);
            let session = resolve_session(&sessions, &reference).map_err(|message| {
                std::iter::once(message)
                    .chain(errors.iter().map(|error| format!("warning: {error}")))
                    .collect::<Vec<_>>()
                    .join("\n")
            })?;
            (session.tool, session.id.clone())
        }
    };
    let to = args.to.unwrap_or_else(|| from.default_convert_target());
    if from == to {
        return Err("--from and --to must be different".to_string());
    }
    let after = match (args.mark_source, args.delete_source) {
        (true, _) => AfterWrite::Mark,
        (_, true) => AfterWrite::Delete,
        _ => AfterWrite::Keep,
    };
    Ok(ConvertOptions {
        id,
        from,
        to,
        source_store: args.source_store,
        target_store: args.target_store,
        target_id: args.target_id,
        cwd: args.cwd,
        title: args.title,
        after,
        dry_run: args.dry_run,
        plan_json: args.plan_json,
        report_json: args.report_json,
        backup: args.backup,
        force: args.force,
        allow_unsupported_version: args.allow_unsupported_version,
    })
}

pub fn build_plan(options: &ConvertOptions) -> AppResult<ConversionPlan> {
    let mut warnings = Vec::new();
    let target = version_report(
        options.to,
        options.target_store.as_ref(),
        None,
        Role::Target,
    )?;
    let name = options.to.name();
    let version = target.version.as_deref().unwrap_or("unknown");
    match &target.support {
        Support::Unsupported { reason } if options.allow_unsupported_version => warnings.push(
            format!("target {name} is unsupported ({reason}); continuing because of --allow-unsupported-version"),
        ),
        Support::Unsupported { reason } => {
            return Err(Box::new(UnsupportedVersion(format!(
                "target {name} is unsupported: {reason}. Pass --allow-unsupported-version to convert anyway"
            ))))
        }
        support if !support.trusted() => {
            warnings.push(format!("target {name} {version}: {}", support.label()))
        }
        _ => {}
    }

    let mut source_session =
        store_for(options.from, options.source_store.as_deref()).read(&options.id)?;
    if source_session.messages.is_empty() {
        return Err(format!(
            "source {} session {} has no readable messages",
            options.from.name(),
            options.id
        )
        .into());
    }
    let source = version_report(
        options.from,
        options.source_store.as_ref(),
        session_written_version(&source_session),
        Role::Source,
    )?;
    // Reading never changes the source store, so source support only warns.
    // The plan counts show what the reader understood.
    if !source.support.trusted() {
        warnings.push(format!(
            "source {} {}: {}; check the mapped and dropped counts",
            options.from.name(),
            source.version.as_deref().unwrap_or("unknown"),
            source.support.label()
        ));
    }
    if let Some(cwd) = &options.cwd {
        source_session.cwd = cwd.clone();
    }
    if let Some(title) = &options.title {
        source_session.title = clean_title(title);
    }
    let mapping = map_session(&source_session, options.to, options.target_id.clone());
    warnings.extend(mapping.warnings);
    Ok(ConversionPlan {
        source,
        target,
        source_session,
        target_session: mapping.session,
        mapped: mapping.mapped,
        dropped: mapping.dropped,
        synthesized: mapping.synthesized,
        warnings,
    })
}

/// `session_version` is the version recorded in the source session; when it
/// is absent, the installed CLI version is checked.
fn version_report(
    tool: Tool,
    store: Option<&PathBuf>,
    session_version: Option<String>,
    role: Role,
) -> AppResult<VersionReport> {
    let cli_version = detect_cli_version(tool);
    let fingerprint = store_for(tool, store.map(PathBuf::as_path)).fingerprint()?;
    let version = session_version.or_else(|| cli_version.clone());
    Ok(VersionReport {
        tool,
        support: support(tool, version.as_deref(), role),
        version,
        cli_version,
        store_version: fingerprint.store_version,
        schema: fingerprint.schema,
    })
}

/// Version recorded by the tool that wrote the session, when the store keeps
/// one. This beats the installed CLI version for old sessions.
fn session_written_version(session: &NativeSession) -> Option<String> {
    let key = match session.tool {
        Tool::Claude => "claude_version",
        Tool::Opencode => "opencode_session_version",
        Tool::Codex => "codex_cli_version",
        Tool::Copilot => "copilot_version",
    };
    session
        .metadata
        .get(key)
        .and_then(Value::as_str)
        .and_then(versions::Version::parse)
        // Some stores write placeholders such as "unknown"; those parse to None.
        .map(|version| version.to_string())
}

struct Mapping {
    session: NativeSession,
    mapped: Vec<String>,
    dropped: Vec<String>,
    synthesized: Vec<String>,
    warnings: Vec<String>,
}

fn map_session(source: &NativeSession, target_tool: Tool, target_id: Option<String>) -> Mapping {
    let mut mapped: Vec<String> = [
        "session id -> provenance",
        "title",
        "cwd",
        "timestamps",
        "messages",
    ]
    .map(str::to_string)
    .to_vec();
    let mut dropped = Vec::new();
    let mut synthesized = Vec::new();
    let mut warnings = Vec::new();
    let mut has_tool_results = false;
    for part in source.messages.iter().flat_map(|message| &message.parts) {
        match part {
            NativePart::Raw { kind, value } => dropped.push(format!(
                "raw part kind `{kind}` ({} bytes)",
                value.to_string().len()
            )),
            NativePart::ToolResult { .. } => has_tool_results = true,
            _ => {}
        }
    }
    if has_tool_results && target_tool == Tool::Opencode {
        warnings.push(
            "tool results are imported into the state of their tool call; a call without a result reads as interrupted"
                .to_string(),
        );
    }
    if source.model.is_some() {
        mapped.push("model".to_string());
    } else {
        synthesized.push("target model defaults to the runtime default".to_string());
    }
    let mut session = source.clone();
    session.tool = target_tool;
    session.id = target_id.unwrap_or_else(|| match target_tool {
        Tool::Opencode => generated_id("ses"),
        Tool::Claude | Tool::Codex | Tool::Copilot => generated_uuid(),
    });
    session
        .metadata
        .insert("imported_from_tool".to_string(), json!(source.tool.name()));
    session
        .metadata
        .insert("imported_from_id".to_string(), json!(source.id));
    Mapping {
        session,
        mapped,
        dropped,
        synthesized,
        warnings,
    }
}

/// Write `plan`, then mark or delete the source, and record the operation.
pub fn convert(
    options: &ConvertOptions,
    plan: ConversionPlan,
    history: &History,
) -> AppResult<Receipt> {
    let target_store = store_for(options.to, options.target_store.as_deref());
    let target_version = plan
        .target
        .cli_version
        .clone()
        .unwrap_or_else(|| newest_known(options.to));
    let provenance = provenance_json(&plan);
    let written = target_store.write(&WriteRequest {
        session: &plan.target_session,
        cli_version: &target_version,
        provenance: &provenance,
        force: options.force,
        backup: options.backup,
    })?;
    if let Err(problem) = verify_readback(&*target_store, &plan.target_session) {
        let footprint = target_store.footprint(&plan.target_session.id)?;
        let trashed = trash(
            &footprint,
            &history.trash_dir(&format!("{}-unverified", options.to.name())),
        )?;
        return Err(format!(
            "the written {} session did not read back ({problem}); moved it to {}",
            options.to.name(),
            trashed.dir.display()
        )
        .into());
    }

    let source_store = store_for(options.from, options.source_store.as_deref());
    let target_cwd = &plan.target_session.cwd;
    let after: AppResult<(Option<Footprint>, Option<Trashed>)> = match options.after {
        AfterWrite::Keep => Ok((None, None)),
        AfterWrite::Mark => {
            let text = format!(
                "This session was migrated to {} by sessiongator. Continue it there with: {}",
                options.to.name(),
                resume_line(options.to, &plan.target_session.id, Some(target_cwd))
            );
            let stamp = plan
                .source
                .version
                .clone()
                .unwrap_or_else(|| newest_known(options.from));
            source_store
                .append_note(
                    &options.id,
                    &Note {
                        text: &text,
                        cli_version: &stamp,
                    },
                )
                .map(|note| (Some(note), None))
        }
        AfterWrite::Delete => source_store.footprint(&options.id).and_then(|footprint| {
            let dir = history.trash_dir(&format!("{}-{}", options.from.name(), options.id));
            trash(&footprint, &dir).map(|trashed| (None, Some(trashed)))
        }),
    };
    let after_error = after.as_ref().err().map(ToString::to_string);
    let (note, source_trash) = after.unwrap_or_default();
    let operation = history.record(Action::Convert {
        source: SessionRef {
            tool: options.from,
            id: options.id.clone(),
            store: options.source_store.clone(),
        },
        target: SessionRef {
            tool: options.to,
            id: plan.target_session.id.clone(),
            store: options.target_store.clone(),
        },
        note,
        source_trash,
    })?;
    if let Some(error) = after_error {
        return Err(format!(
            "wrote {}:{}, but could not change the source session: {error}. `sessiongator undo {} --target` removes the new session",
            options.to.name(),
            plan.target_session.id,
            operation.id
        )
        .into());
    }
    Ok(Receipt {
        reference: written.reference,
        backup: written.backup,
        operation,
        plan,
    })
}

/// Read the written session back through its store and compare what every
/// target keeps: title, cwd, and the user and assistant messages that hold
/// text. Tool-only and control messages differ per target by design.
fn verify_readback(store: &dyn Store, written: &NativeSession) -> Result<(), String> {
    let back = store.read(&written.id).map_err(|error| error.to_string())?;
    let text_messages = |session: &NativeSession| {
        session
            .messages
            .iter()
            .filter(|message| matches!(message.role, NativeRole::User | NativeRole::Assistant))
            .filter(|message| !parts_text(&message.parts).trim().is_empty())
            .count()
    };
    let checks = [
        ("title", back.title == written.title),
        ("cwd", back.cwd == written.cwd),
        (
            "text message count",
            text_messages(&back) == text_messages(written),
        ),
    ];
    match checks.iter().find(|(_, ok)| !ok) {
        Some((field, _)) => Err(format!("{field} differs")),
        None => Ok(()),
    }
}

fn print_summary(plan: &ConversionPlan, options: &ConvertOptions) {
    println!(
        "{}:{} -> {}:{}",
        plan.source.tool.name(),
        plan.source_session.id,
        plan.target.tool.name(),
        plan.target_session.id
    );
    println!("title:    {}", plan.source_session.title);
    println!("cwd:      {}", plan.target_session.cwd);
    println!("messages: {}", plan.source_session.messages.len());
    for (label, report) in [("source:", &plan.source), ("target:", &plan.target)] {
        println!(
            "{label:<9} {} {} ({})",
            report.tool.name(),
            report.version.as_deref().unwrap_or("unknown"),
            report.support.label()
        );
    }
    println!(
        "mapped {}, dropped {}, synthesized {}",
        plan.mapped.len(),
        plan.dropped.len(),
        plan.synthesized.len()
    );
    for dropped in &plan.dropped {
        println!("  dropped: {dropped}");
    }
    for warning in &plan.warnings {
        println!("  warning: {warning}");
    }
    let mut command = format!(
        "sessiongator convert --from {} --to {} --id {}",
        options.from.name(),
        options.to.name(),
        options.id
    );
    match options.after {
        AfterWrite::Keep => {}
        AfterWrite::Mark => command.push_str(" --mark-source"),
        AfterWrite::Delete => command.push_str(" --delete-source"),
    }
    if options.allow_unsupported_version {
        command.push_str(" --allow-unsupported-version");
    }
    println!("write with: {command}");
}

fn version_json(report: &VersionReport) -> Value {
    json!({
        "tool": report.tool.name(),
        "version": report.version,
        "cliVersion": report.cli_version,
        "storeVersion": report.store_version,
        "schemaFingerprint": report.schema,
        "support": report.support.json(),
    })
}

fn session_json(session: &NativeSession) -> Value {
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

pub fn plan_json(plan: &ConversionPlan) -> Value {
    json!({
        "source": version_json(&plan.source),
        "target": version_json(&plan.target),
        "sourceSession": session_json(&plan.source_session),
        "targetSession": session_json(&plan.target_session),
        "mapped": plan.mapped,
        "dropped": plan.dropped,
        "synthesized": plan.synthesized,
        "warnings": plan.warnings,
    })
}

fn receipt_json(receipt: &Receipt) -> Value {
    let (marked, source_deleted) = match &receipt.operation.action {
        Action::Convert {
            note, source_trash, ..
        } => (note.is_some(), source_trash.is_some()),
        Action::Delete { .. } => (false, false),
    };
    json!({
        "targetId": receipt.plan.target_session.id,
        "targetRef": receipt.reference,
        "backup": receipt.backup.as_ref().map(|path| path.display().to_string()),
        "operation": receipt.operation.id,
        "sourceMarked": marked,
        "sourceDeleted": source_deleted,
        "report": plan_json(&receipt.plan),
    })
}

fn provenance_json(plan: &ConversionPlan) -> Value {
    json!({
        "sourceTool": plan.source_session.tool.name(),
        "sourceId": plan.source_session.id,
        "sourceVersion": plan.source.version,
        "targetTool": plan.target_session.tool.name(),
        "targetVersion": plan.target.version,
        "convertedAtMs": now_ms(),
        "mappingProfile": "native-session-import-v1",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{turns, NativeMessage};
    use crate::stores::opencode::create_test_v2_db;
    use crate::stores::tests::temp_dir;

    const FIXTURE_ID: &str = "11111111-2222-4333-8444-555555555555";

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/native-import/claude/2.1.199/basic-text/source")
    }

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: ConvertArgs,
    }

    fn parse(args: &[&str]) -> Result<ConvertOptions, String> {
        use clap::Parser;
        let cli = Cli::try_parse_from(std::iter::once("convert").chain(args.iter().copied()))
            .map_err(|error| error.to_string())?;
        options_from_args(cli.args)
    }

    fn options(
        from: Tool,
        id: &str,
        source_store: PathBuf,
        to: Tool,
        target_store: PathBuf,
    ) -> ConvertOptions {
        ConvertOptions {
            id: id.to_string(),
            from,
            to,
            source_store: Some(source_store),
            target_store: Some(target_store),
            target_id: None,
            cwd: None,
            title: None,
            after: AfterWrite::Keep,
            dry_run: false,
            plan_json: false,
            report_json: false,
            backup: false,
            force: false,
            allow_unsupported_version: true,
        }
    }

    /// The fixture copied into a fresh store, so it can be marked or deleted.
    fn claude_fixture_copy() -> PathBuf {
        let root = temp_dir("claude-fixture");
        let project = root.join("projects/-tmp-sessiongator-demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::copy(
            fixture_root().join(format!(
                "projects/-tmp-sessiongator-demo/{FIXTURE_ID}.jsonl"
            )),
            project.join(format!("{FIXTURE_ID}.jsonl")),
        )
        .unwrap();
        root
    }

    fn comparable(session: &NativeSession) -> Value {
        json!({
            "title": session.title,
            "cwd": session.cwd,
            "model": session.model.as_ref().map(model_json),
            "messages": session.messages.iter().map(|message| json!({
                "role": message.role.name(),
                "parts": crate::native::parts_to_json(&message.parts),
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn parses_explicit_args_without_a_session_lookup() {
        let parsed = parse(&[
            "--id",
            "abc",
            "--from",
            "claude",
            "--to",
            "opencode",
            "--dry-run",
        ])
        .unwrap();
        assert_eq!(
            (parsed.id.as_str(), parsed.from, parsed.to),
            ("abc", Tool::Claude, Tool::Opencode)
        );
        assert!(parsed.dry_run && !parsed.backup);
        assert_eq!(parsed.after, AfterWrite::Keep);
        let defaulted = parse(&["--id", "abc", "--from", "codex", "--delete-source"]).unwrap();
        assert_eq!(defaulted.to, Tool::Claude);
        assert_eq!(defaulted.after, AfterWrite::Delete);
    }

    #[test]
    fn rejects_invalid_arg_combinations() {
        assert!(parse(&["--id", "abc", "--from", "claude", "--to", "claude"]).is_err());
        assert!(parse(&["--id", "abc", "--source-store", "/tmp/x"]).is_err());
        assert!(parse(&["abc", "--id", "abc"]).is_err());
        assert!(parse(&[
            "--id",
            "a",
            "--from",
            "claude",
            "--mark-source",
            "--delete-source"
        ])
        .is_err());
    }

    #[test]
    fn maps_every_cross_tool_pair_without_drops() {
        let source = store_for(Tool::Claude, Some(&fixture_root()))
            .read(FIXTURE_ID)
            .unwrap();
        for to in Tool::ALL.into_iter().filter(|tool| *tool != Tool::Claude) {
            let mapping = map_session(&source, to, Some("target".to_string()));
            assert_eq!(mapping.session.tool, to);
            assert_eq!(mapping.session.id, "target");
            assert!(mapping.mapped.iter().any(|item| item == "messages"));
            assert!(mapping.dropped.is_empty());
        }
    }

    #[test]
    fn fixture_roundtrips_through_every_target_and_back_to_claude() {
        let source = store_for(Tool::Claude, Some(&fixture_root()))
            .read(FIXTURE_ID)
            .unwrap();
        for via in [Tool::Opencode, Tool::Codex, Tool::Copilot] {
            let root = temp_dir(&format!("roundtrip-{}", via.name()));
            let history = History::at(&root.join("state"));
            let via_store = match via {
                Tool::Opencode => {
                    let db = root.join("opencode.db");
                    create_test_v2_db(&db);
                    db
                }
                _ => root.join(via.name()),
            };
            let forward = options(
                Tool::Claude,
                FIXTURE_ID,
                fixture_root(),
                via,
                via_store.clone(),
            );
            let receipt = convert(&forward, build_plan(&forward).unwrap(), &history).unwrap();
            let via_id = receipt.plan.target_session.id.clone();
            let back = options(via, &via_id, via_store, Tool::Claude, root.join("claude"));
            let receipt = convert(&back, build_plan(&back).unwrap(), &history).unwrap();
            let roundtrip = store_for(Tool::Claude, Some(&root.join("claude")))
                .read(&receipt.plan.target_session.id)
                .unwrap();
            if via == Tool::Opencode {
                // OpenCode keeps every native part, so the round trip is exact.
                assert_eq!(comparable(&roundtrip), comparable(&source));
            }
            assert_eq!(turns(&roundtrip), turns(&source), "via {}", via.name());
            assert_eq!(roundtrip.cwd, source.cwd);
        }
    }

    #[test]
    fn mark_source_appends_a_note_that_undo_removes() {
        let root = temp_dir("mark");
        let source_root = claude_fixture_copy();
        let history = History::at(&root.join("state"));
        let mut marked = options(
            Tool::Claude,
            FIXTURE_ID,
            source_root.clone(),
            Tool::Codex,
            root.join("codex"),
        );
        marked.after = AfterWrite::Mark;
        let receipt = convert(&marked, build_plan(&marked).unwrap(), &history).unwrap();
        let source = store_for(Tool::Claude, Some(&source_root));
        let note = source.transcript(FIXTURE_ID).unwrap().pop().unwrap().text;
        assert!(note.contains("migrated to codex"), "{note}");
        assert!(note.contains(&receipt.plan.target_session.id), "{note}");

        history.undo(&receipt.operation, true).unwrap();
        let after = source.transcript(FIXTURE_ID).unwrap();
        assert!(!after.iter().any(|turn| turn.text.contains("migrated to")));
        assert!(store_for(Tool::Codex, Some(&root.join("codex")))
            .read(&receipt.plan.target_session.id)
            .is_err());
    }

    #[test]
    fn delete_source_moves_it_to_the_trash_and_undo_restores_it() {
        let root = temp_dir("delete");
        let source_root = claude_fixture_copy();
        let history = History::at(&root.join("state"));
        let mut deleting = options(
            Tool::Claude,
            FIXTURE_ID,
            source_root.clone(),
            Tool::Copilot,
            root.join("copilot"),
        );
        deleting.after = AfterWrite::Delete;
        let receipt = convert(&deleting, build_plan(&deleting).unwrap(), &history).unwrap();
        let source = store_for(Tool::Claude, Some(&source_root));
        assert!(source.read(FIXTURE_ID).is_err());

        history.undo(&history.find(None).unwrap(), false).unwrap();
        assert_eq!(source.read(FIXTURE_ID).unwrap().messages.len(), 3);
        assert!(store_for(Tool::Copilot, Some(&root.join("copilot")))
            .read(&receipt.plan.target_session.id)
            .is_ok());
    }

    #[test]
    fn readback_check_rejects_a_session_that_reads_back_differently() {
        let store = store_for(Tool::Claude, Some(&fixture_root()));
        let mut written = store.read(FIXTURE_ID).unwrap();
        assert_eq!(verify_readback(&*store, &written), Ok(()));
        written.title = "another title".to_string();
        assert_eq!(
            verify_readback(&*store, &written),
            Err("title differs".to_string())
        );
        written = store.read(FIXTURE_ID).unwrap();
        written.messages.remove(0);
        assert!(verify_readback(&*store, &written).is_err());
    }

    #[test]
    fn a_source_without_messages_is_an_error() {
        let root = claude_fixture_copy();
        let path = root.join(format!(
            "projects/-tmp-sessiongator-demo/{FIXTURE_ID}.jsonl"
        ));
        std::fs::write(&path, r#"{"type":"ai-title","aiTitle":"empty"}"#).unwrap();
        let empty = options(
            Tool::Claude,
            FIXTURE_ID,
            root,
            Tool::Codex,
            temp_dir("empty-target"),
        );
        let error = build_plan(&empty).unwrap_err();
        assert!(
            error.to_string().contains("no readable messages"),
            "{error}"
        );
    }

    /// `expected-opencode/plan-summary.json` is the checked-in contract for
    /// what the fixture plan reports.
    #[test]
    fn fixture_plan_matches_the_expected_summary() {
        let expected: Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_root().join("../expected-opencode/plan-summary.json"))
                .unwrap(),
        )
        .unwrap();
        let db = temp_dir("expected-summary").join("opencode.db");
        let plan = build_plan(&options(
            Tool::Claude,
            FIXTURE_ID,
            fixture_root(),
            Tool::Opencode,
            db,
        ))
        .unwrap();
        let report = plan_json(&plan);
        let source = &report["sourceSession"];
        let actual = json!({
            "sourceTool": source["tool"],
            "targetTool": report["targetSession"]["tool"],
            "sourceId": source["id"],
            "title": source["title"],
            "cwd": source["cwd"],
            "messageCount": source["messageCount"],
            "partCounts": source["partCounts"],
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn plan_reports_parts_and_support() {
        let mut plan_options = options(
            Tool::Claude,
            FIXTURE_ID,
            fixture_root(),
            Tool::Codex,
            temp_dir("plan"),
        );
        plan_options.target_id = Some("t".to_string());
        let plan = build_plan(&plan_options).unwrap();
        let value = plan_json(&plan);
        assert_eq!(value["sourceSession"]["messageCount"], 3);
        assert_eq!(value["sourceSession"]["partCounts"]["tool_call"], 1);
        assert_eq!(value["source"]["version"], "2.1.199");
        assert!(value["source"]["support"]["status"].is_string());
        assert_eq!(value["targetSession"]["id"], "t");
        let _ = NativeMessage::text(NativeRole::User, 0, String::new());
    }
}
