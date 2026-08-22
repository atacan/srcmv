#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! Command grammar, protocol orchestration, and output discipline for srcmv.
//!
//! Preview and inspection use immutable workspace snapshots coordinated by the
//! existing diagnostic lock when present. Multi-target commit and recovery use
//! the persistent transaction engine.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;

use clap::error::ErrorKind;
use clap::{ArgAction, Args, Parser, Subcommand};
use serde_json::json;
use srcmv_core::{Operation, OutputChange, Precondition, ResourceBudget, plan};
use srcmv_fs::LEGACY_CONTROL_NAME;
use srcmv_fs::{
    DiagnosticLock, FsError, InspectedState, RecoveryEntryKind, RequiredPathState, SnapshotLimits,
    SnapshotRequirement, Workspace, capture_startup_umask,
};
use srcmv_protocol::{
    CapabilitiesResponse, CommitResponse, ErrorCode, ErrorDto, InspectPathResponse,
    InspectResponse, MAX_OPERATION_PATHS, MAX_PATH_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    OutputResponse, ProtocolVersionResponse, RecoveryEntryResponse, RecoveryListResponse,
    RecoveryStatusResponse, ResolvedOperationResponse, SelectionCapabilitiesResponse, WarningCode,
    WarningDto, escape_terminal_text, parse_request, parse_sha256, redact_path, to_json_line,
};

mod outline;
mod preview;
mod select;

static STARTUP_UMASK: OnceLock<u32> = OnceLock::new();

/// Parses process arguments, runs the selected command, and returns its exit status.
///
/// Output is written according to the command's JSON or human-mode contract. This
/// Inspection and preview are read-only. Commit and recovery mutate only through
/// the persistent transaction engine.
#[must_use]
pub fn run() -> ExitCode {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let status = run_with_io(env::args_os(), &mut stdin, &mut stdout, &mut stderr);
    ExitCode::from(status)
}

#[derive(Debug, Parser)]
#[command(
    name = "srcmv",
    version,
    about = "Move or copy exact bytes already present in workspace files",
    disable_help_subcommand = true
)]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    workspace: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect(InspectArgs),
    Select(select::SelectArgs),
    Outline(outline::OutlineArgs),
    Apply(ApplyArgs),
    Recover(RecoverArgs),
    Capabilities(JsonOnlyArgs),
    SelectionCapabilities(JsonOnlyArgs),
    ProtocolVersion(JsonOnlyArgs),
}

#[derive(Debug, Args)]
struct InspectArgs {
    #[arg(long = "path", required = true, value_name = "RELATIVE")]
    paths: Vec<String>,
    #[arg(long, required = true, action = ArgAction::SetTrue)]
    json: bool,
}

#[derive(Debug, Args)]
struct ApplyArgs {
    #[arg(long, value_name = "FILE|-", required = true)]
    request: String,
    #[command(flatten)]
    mode: ApplyMode,
    #[arg(long, value_name = "sha256:DIGEST", requires = "commit")]
    expect_plan: Option<String>,
    #[arg(long, conflicts_with = "expect_plan", requires = "commit")]
    accept_current_plan: bool,
    #[arg(long)]
    json: bool,
    #[arg(long, requires = "preview")]
    no_diff: bool,
    #[arg(
        long,
        requires = "preview",
        help = "Include complete typed review metadata under diff.summary.review"
    )]
    summary: bool,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct ApplyMode {
    #[arg(long)]
    preview: bool,
    #[arg(long)]
    commit: bool,
}

#[derive(Debug, Args)]
struct RecoverArgs {
    #[arg(
        value_name = "ID",
        required_unless_present = "list",
        conflicts_with = "list"
    )]
    id: Option<String>,
    #[command(flatten)]
    action: RecoveryAction,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct RecoveryAction {
    #[arg(long, conflicts_with = "id")]
    list: bool,
    #[arg(long, requires = "id")]
    status: bool,
    #[arg(long, requires = "id")]
    complete: bool,
    #[arg(long, requires = "id")]
    rollback: bool,
}

#[derive(Debug, Args)]
struct JsonOnlyArgs {
    #[arg(long, required = true, action = ArgAction::SetTrue)]
    json: bool,
}

fn run_with_io<I, T>(
    arguments: I,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let startup_umask = *STARTUP_UMASK.get_or_init(capture_startup_umask);
    let arguments = arguments
        .into_iter()
        .map(Into::into)
        .collect::<Vec<OsString>>();
    let json_requested = arguments.iter().any(|argument| argument == "--json");

    let cli = match Cli::try_parse_from(&arguments) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            return if stdout.write_all(error.to_string().as_bytes()).is_ok() {
                0
            } else {
                8
            };
        }
        Err(error) => {
            let report = ErrorDto::new(
                ErrorCode::InvalidCli,
                "the command line does not match the srcmv grammar",
                BTreeMap::from([("reason".to_string(), json!(error.to_string()))]),
            );
            return render_error(&report, json_requested, stdout, stderr);
        }
    };

    match execute(cli, stdin, startup_umask) {
        Ok(response) => render_success(&response, stdout, stderr),
        Err(CommandFailure::Edit(report, json)) => render_error(&report, json, stdout, stderr),
        Err(CommandFailure::Selection(failure)) => render_selection_error(&failure, stdout, stderr),
        Err(CommandFailure::Outline(failure)) => render_outline_error(&failure, stdout, stderr),
    }
}

enum CommandFailure {
    Edit(ErrorDto, bool),
    Selection(select::SelectionFailure),
    Outline(outline::OutlineFailure),
}

fn execute(cli: Cli, stdin: &mut dyn Read, startup_umask: u32) -> Result<String, CommandFailure> {
    let has_workspace = cli.workspace.is_some();
    match cli.command {
        Command::Capabilities(arguments) => {
            reject_workspace_for_target_independent(has_workspace, arguments.json)
                .map_err(|(report, json)| CommandFailure::Edit(report, json))?;
            serialize_success(&CapabilitiesResponse::v0_1_0(), arguments.json)
                .map_err(|(report, json)| CommandFailure::Edit(report, json))
        }
        Command::SelectionCapabilities(arguments) => {
            reject_workspace_for_target_independent(has_workspace, arguments.json)
                .map_err(|(report, json)| CommandFailure::Edit(report, json))?;
            serialize_success(&SelectionCapabilitiesResponse::current(), arguments.json)
                .map_err(|(report, json)| CommandFailure::Edit(report, json))
        }
        Command::ProtocolVersion(arguments) => {
            reject_workspace_for_target_independent(has_workspace, arguments.json)
                .map_err(|(report, json)| CommandFailure::Edit(report, json))?;
            serialize_success(&ProtocolVersionResponse::current(), arguments.json)
                .map_err(|(report, json)| CommandFailure::Edit(report, json))
        }
        Command::Inspect(arguments) => {
            validate_inspect_paths(&arguments.paths)
                .map_err(|report| CommandFailure::Edit(report, arguments.json))?;
            execute_inspect(cli.workspace.as_deref(), arguments)
                .map_err(|(report, json)| CommandFailure::Edit(report, json))
        }
        Command::Select(arguments) => {
            select::execute(cli.workspace.as_deref(), arguments).map_err(CommandFailure::Selection)
        }
        Command::Outline(arguments) => {
            outline::execute(cli.workspace.as_deref(), arguments).map_err(CommandFailure::Outline)
        }
        Command::Apply(arguments) => {
            execute_apply(cli.workspace.as_deref(), arguments, stdin, startup_umask)
                .map_err(|(report, json)| CommandFailure::Edit(report, json))
        }
        Command::Recover(arguments) => execute_recovery(cli.workspace.as_deref(), arguments)
            .map_err(|(report, json)| CommandFailure::Edit(report, json)),
    }
}

fn execute_inspect(
    workspace_path: Option<&std::path::Path>,
    arguments: InspectArgs,
) -> Result<String, (ErrorDto, bool)> {
    let workspace = Workspace::open(workspace_path.unwrap_or_else(|| std::path::Path::new(".")))
        .map_err(|error| (filesystem_error(error), arguments.json))?;
    let (diagnostic_lock, warnings) = diagnostic_context(&workspace)
        .map_err(|error| (filesystem_error(error), arguments.json))?;
    let inspections = workspace
        .inspect(&arguments.paths, SnapshotLimits::default())
        .map_err(|error| (filesystem_error(error), arguments.json))?;
    let paths = inspections
        .into_iter()
        .map(|inspection| match inspection.state {
            InspectedState::Existing {
                digest,
                byte_length,
                line_count,
                identity_hash,
            } => InspectPathResponse::existing(
                inspection.path.value,
                digest,
                byte_length,
                line_count,
                identity_hash,
            ),
            InspectedState::Absent => InspectPathResponse::absent(inspection.path.value),
        })
        .collect();
    let result = serialize_success(&InspectResponse::new(paths, warnings), arguments.json);
    drop(diagnostic_lock);
    result
}

fn diagnostic_context(
    workspace: &Workspace,
) -> Result<(Option<DiagnosticLock>, Vec<WarningDto>), FsError> {
    let lock = workspace.diagnostic_lock()?;
    let Some(lock) = lock else {
        return Ok((
            None,
            vec![WarningDto::new(
                WarningCode::ObservationMayBeStale,
                "no existing srcmv lock coordinated this read-only observation",
                BTreeMap::new(),
            )],
        ));
    };
    let observation = lock.scan()?;
    let transaction_ids = observation
        .entries()
        .iter()
        .filter(|entry| entry.kind() != RecoveryEntryKind::CleanupOnly)
        .map(|entry| entry.transaction_id().to_owned())
        .collect::<Vec<_>>();
    if !transaction_ids.is_empty() {
        return Err(FsError::TransactionRecoveryRequired { transaction_ids });
    }
    Ok((Some(lock), Vec::new()))
}

fn filesystem_error(error: FsError) -> ErrorDto {
    match error {
        FsError::UnsupportedPlatform => ErrorDto::new(
            ErrorCode::UnsupportedPlatform,
            "workspace inspection requires Linux or macOS",
            BTreeMap::new(),
        ),
        FsError::WorkspaceRootNotDirectory => ErrorDto::new(
            ErrorCode::InvalidRequest,
            "the selected workspace root is not a directory",
            BTreeMap::from([("reason".to_string(), json!("workspace_not_directory"))]),
        ),
        FsError::InvalidPath { path, reason } => ErrorDto::new(
            ErrorCode::InvalidRequest,
            "an inspection path is invalid",
            BTreeMap::from([
                ("path".to_string(), json!(redact_path(&path))),
                ("reason".to_string(), json!(reason)),
            ]),
        ),
        FsError::SymlinkNotAllowed { path } => ErrorDto::new(
            ErrorCode::SymlinkNotAllowed,
            "an inspection path traverses or names a symbolic link",
            BTreeMap::from([("path".to_string(), json!(path))]),
        ),
        FsError::UnsupportedFileType { path } => ErrorDto::new(
            ErrorCode::UnsupportedFileType,
            "an inspection path is not a regular file",
            BTreeMap::from([("path".to_string(), json!(path))]),
        ),
        FsError::PathNotFound { path } => ErrorDto::new(
            ErrorCode::InvalidRequest,
            "an inspection path does not exist",
            BTreeMap::from([("path".to_string(), json!(path))]),
        ),
        FsError::PreconditionFailed {
            path,
            expected,
            actual,
        } => {
            let mut context = BTreeMap::from([("path".to_string(), json!(path))]);
            context.insert(
                "expected".to_string(),
                json!(expected.map(srcmv_core::Sha256Digest::to_prefixed_hex)),
            );
            context.insert(
                "actual".to_string(),
                json!(actual.map(srcmv_core::Sha256Digest::to_prefixed_hex)),
            );
            ErrorDto::new(
                ErrorCode::PreconditionFailed,
                "a path precondition does not match the stable workspace state",
                context,
            )
        }
        FsError::IncompatiblePrecondition { path } => ErrorDto::new(
            ErrorCode::EditConflict,
            "one path has incompatible preconditions",
            BTreeMap::from([
                ("path".to_string(), json!(path)),
                ("reason".to_string(), json!("incompatible_preconditions")),
            ]),
        ),
        FsError::FileAlias {
            first_path,
            second_path,
        } => ErrorDto::new(
            ErrorCode::FileAlias,
            "distinct paths identify the same existing file",
            BTreeMap::from([
                ("first_path".to_string(), json!(first_path)),
                ("second_path".to_string(), json!(second_path)),
            ]),
        ),
        FsError::FileChanged { path, attempts } => ErrorDto::new(
            ErrorCode::FileChanged,
            "a file remained unstable during bounded snapshot acquisition",
            BTreeMap::from([
                ("attempts".to_string(), json!(attempts)),
                ("path".to_string(), json!(path)),
            ]),
        ),
        FsError::ResourceLimitExceeded {
            resource,
            actual,
            limit,
        }
        | FsError::Core(srcmv_core::CoreError::ResourceLimitExceeded {
            resource,
            actual,
            limit,
        }) => limit_error(resource, actual, limit),
        FsError::CrossDeviceTransaction => ErrorDto::new(
            ErrorCode::CrossDeviceTransaction,
            "the control directory and changed target parent are on different filesystems",
            BTreeMap::new(),
        ),
        FsError::NoReplaceUnavailable => ErrorDto::new(
            ErrorCode::NoReplaceUnavailable,
            "the required no-replace rename primitive is unavailable",
            BTreeMap::new(),
        ),
        FsError::UnsupportedFilesystem { filesystem } => ErrorDto::new(
            ErrorCode::UnsupportedFilesystem,
            "commit requires a qualified local ext4 or APFS filesystem",
            BTreeMap::from([("filesystem".to_owned(), json!(filesystem))]),
        ),
        FsError::TransactionBusy => transaction_busy_error(),
        FsError::TransactionRecoveryRequired { transaction_ids } => ErrorDto::new(
            ErrorCode::TransactionRecoveryRequired,
            "unfinished transactions require explicit recovery",
            BTreeMap::from([("transaction_ids".to_string(), json!(transaction_ids))]),
        ),
        FsError::TransactionNotFound { transaction_id } => ErrorDto::new(
            ErrorCode::TransactionNotFound,
            "the requested transaction does not exist",
            BTreeMap::from([("transaction_id".to_string(), json!(transaction_id))]),
        ),
        FsError::RecoveryActionNotAllowed {
            transaction_id,
            reason,
        } => ErrorDto::new(
            ErrorCode::RecoveryActionNotAllowed,
            "the requested recovery action is not safe in the current state",
            BTreeMap::from([
                ("reason".to_string(), json!(reason)),
                ("transaction_id".to_string(), json!(transaction_id)),
            ]),
        ),
        FsError::ControlDirectoryInvalid { reason } => ErrorDto::new(
            ErrorCode::ControlDirectoryInvalid,
            "the workspace control tree is invalid",
            BTreeMap::from([("reason".to_string(), json!(reason))]),
        ),
        FsError::LegacyControlState => ErrorDto::new(
            ErrorCode::ControlDirectoryInvalid,
            format!(
                "{LEGACY_CONTROL_NAME} holds unfinished transaction state from the former \
                 product identity; finish or remove it with the former tool before running \
                 srcmv in this workspace"
            ),
            BTreeMap::from([
                ("reason".to_string(), json!("legacy_control_state")),
                ("path".to_string(), json!(LEGACY_CONTROL_NAME)),
            ]),
        ),
        FsError::TransactionRecordCorrupt {
            transaction_id,
            reason,
        } => {
            let mut context = BTreeMap::from([("reason".to_string(), json!(reason))]);
            if let Some(transaction_id) = transaction_id {
                context.insert("transaction_id".to_string(), json!(transaction_id));
            }
            ErrorDto::new(
                ErrorCode::TransactionRecordCorrupt,
                "a transaction record is corrupt",
                context,
            )
        }
        FsError::RecoveryConflict { reason } => ErrorDto::new(
            ErrorCode::RecoveryConflict,
            "filesystem observations conflict with the transaction journal",
            BTreeMap::from([("reason".to_string(), json!(reason))]),
        ),
        FsError::Io {
            operation,
            path,
            kind,
        } => {
            let mut context = BTreeMap::from([
                ("io_kind".to_string(), json!(format!("{kind:?}"))),
                ("operation".to_string(), json!(operation)),
            ]);
            if let Some(path) = path {
                context.insert("path".to_string(), json!(path));
            }
            ErrorDto::new(ErrorCode::IoError, "workspace operation failed", context)
        }
        FsError::Core(srcmv_core::CoreError::EditConflict {
            reason,
            operation_index,
        }) => ErrorDto::new(
            ErrorCode::EditConflict,
            "the requested edits conflict under immutable-snapshot semantics",
            BTreeMap::from([
                ("operation_index".to_string(), json!(operation_index)),
                ("reason".to_string(), json!(reason)),
            ]),
        ),
        FsError::Core(srcmv_core::CoreError::HardLinkNotSupported { path, link_count }) => {
            ErrorDto::new(
                ErrorCode::HardLinkNotSupported,
                "a changing output has multiple hard links",
                BTreeMap::from([
                    ("link_count".to_string(), json!(link_count)),
                    ("path".to_string(), json!(path.value)),
                ]),
            )
        }
        FsError::Core(error) => ErrorDto::new(
            ErrorCode::InternalError,
            "the core planning model rejected acquired data",
            BTreeMap::from([("reason".to_string(), json!(error.to_string()))]),
        ),
        FsError::InternalInvariant { invariant } => ErrorDto::new(
            ErrorCode::InternalError,
            "an internal workspace inspection invariant failed",
            BTreeMap::from([("invariant".to_string(), json!(invariant))]),
        ),
        _ => ErrorDto::new(
            ErrorCode::InternalError,
            "an unrecognized filesystem error occurred",
            BTreeMap::new(),
        ),
    }
}

fn transaction_busy_error() -> ErrorDto {
    ErrorDto::new(
        ErrorCode::TransactionBusy,
        "an incompatible workspace lock is held; wait and retry; never bypass or remove the lock",
        BTreeMap::from([
            ("lock_state".to_owned(), json!("contended")),
            ("recovery_required".to_owned(), json!("unknown")),
            ("safe_next_action".to_owned(), json!("wait_then_retry")),
        ]),
    )
}

fn execute_recovery(
    workspace_path: Option<&std::path::Path>,
    arguments: RecoverArgs,
) -> Result<String, (ErrorDto, bool)> {
    if let Some(transaction_id) = arguments.id.as_deref() {
        validate_transaction_id_argument(transaction_id)
            .map_err(|report| (report, arguments.json))?;
    }
    let workspace = Workspace::open(workspace_path.unwrap_or_else(|| std::path::Path::new(".")))
        .map_err(|error| (filesystem_error(error), arguments.json))?;
    if arguments.action.list {
        let observation = workspace
            .recovery_list()
            .map_err(|error| (filesystem_error(error), arguments.json))?;
        if arguments.json {
            let entries = observation
                .entries()
                .iter()
                .map(recovery_entry_response)
                .collect();
            return serialize_success(&RecoveryListResponse::new(entries), true);
        }
        let mut output = String::new();
        for entry in observation.entries() {
            output.push_str(&format!(
                "{} {} visibility={} [{}]\n",
                entry.transaction_id(),
                entry.kind().as_str(),
                entry.visibility(),
                entry.actions().join(",")
            ));
        }
        return Ok(output);
    }

    let transaction_id = arguments.id.as_deref().ok_or_else(|| {
        (
            ErrorDto::new(
                ErrorCode::InvalidCli,
                "recovery requires a transaction ID",
                BTreeMap::new(),
            ),
            arguments.json,
        )
    })?;
    if arguments.action.status {
        let entry = workspace
            .recovery_status(transaction_id)
            .map_err(|error| (filesystem_error(error), arguments.json))?;
        if arguments.json {
            return serialize_success(
                &RecoveryStatusResponse::new(recovery_entry_response(&entry)),
                true,
            );
        }
        return Ok(format!(
            "{} {} visibility={} [{}]\n",
            entry.transaction_id(),
            entry.kind().as_str(),
            entry.visibility(),
            entry.actions().join(",")
        ));
    }
    if arguments.action.rollback {
        let outcome = workspace
            .recovery_rollback(transaction_id)
            .map_err(|error| (filesystem_error(error), arguments.json))?;
        let completed = RecoveryEntryResponse::new(
            transaction_id,
            "cleanup_only",
            std::iter::empty::<&str>(),
            "all_original",
        );
        if arguments.json {
            return serialize_success(&RecoveryStatusResponse::new(completed), true);
        }
        return Ok(format!(
            "{} {}\n",
            outcome.transaction_id(),
            outcome.state()
        ));
    }
    let outcome = workspace
        .recovery_complete(transaction_id)
        .map_err(|error| (filesystem_error(error), arguments.json))?;
    let completed = RecoveryEntryResponse::new(
        transaction_id,
        "cleanup_only",
        std::iter::empty::<&str>(),
        "all_planned",
    );
    if arguments.json {
        return serialize_success(&RecoveryStatusResponse::new(completed), true);
    }
    Ok(format!(
        "{} {}\n",
        outcome.transaction_id(),
        outcome.state()
    ))
}

fn recovery_entry_response(entry: &srcmv_fs::RecoveryEntry) -> RecoveryEntryResponse {
    RecoveryEntryResponse::new(
        entry.transaction_id(),
        entry.kind().as_str(),
        entry.actions().iter().copied(),
        entry.visibility(),
    )
}

fn validate_transaction_id_argument(transaction_id: &str) -> Result<(), ErrorDto> {
    if transaction_id.len() == 32
        && transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ErrorDto::new(
            ErrorCode::InvalidRequest,
            "the transaction ID must be exactly 32 lowercase hexadecimal characters",
            BTreeMap::from([("reason".to_string(), json!("invalid_transaction_id"))]),
        ))
    }
}

fn execute_apply(
    workspace_path: Option<&std::path::Path>,
    arguments: ApplyArgs,
    stdin: &mut dyn Read,
    startup_umask: u32,
) -> Result<String, (ErrorDto, bool)> {
    if arguments.mode.commit && arguments.expect_plan.is_none() && !arguments.accept_current_plan {
        return Err((
            ErrorDto::new(
                ErrorCode::ExpectedPlanRequired,
                "commit requires exactly one expected-plan policy",
                BTreeMap::new(),
            ),
            arguments.json,
        ));
    }

    let expected_plan = arguments
        .expect_plan
        .as_deref()
        .map(|expected| parse_sha256(expected, "--expect-plan"))
        .transpose()
        .map_err(|error| (error.into_report(), arguments.json))?;

    let request =
        read_request(&arguments.request, stdin).map_err(|report| (report, arguments.json))?;
    let batch = parse_request(&request).map_err(|error| (error.into_report(), arguments.json))?;

    let workspace = Workspace::open(workspace_path.unwrap_or_else(|| std::path::Path::new(".")))
        .map_err(|error| (filesystem_error(error), arguments.json))?;
    if arguments.mode.commit {
        return execute_commit(
            &workspace,
            &batch,
            expected_plan,
            startup_umask,
            arguments.json,
        );
    }
    let (diagnostic_lock, warnings) = diagnostic_context(&workspace)
        .map_err(|error| (filesystem_error(error), arguments.json))?;
    let requirements = snapshot_requirements(&batch);
    let snapshot = workspace
        .acquire_snapshot(&requirements, SnapshotLimits::default())
        .map_err(|error| (filesystem_error(error), arguments.json))?;
    let edit_plan = plan(&snapshot, &batch, ResourceBudget::default())
        .map_err(|error| (filesystem_error(error.into()), arguments.json))?;
    let report = preview::build_preview(
        &snapshot,
        &edit_plan,
        workspace.identity_hash(),
        arguments.no_diff,
        arguments.summary,
        warnings,
    )
    .map_err(|error| (filesystem_error(error), arguments.json))?;
    let result = if arguments.json {
        serialize_preview(&report.response)
    } else {
        Ok(report.human)
    };
    drop(diagnostic_lock);
    result
}

fn execute_commit(
    workspace: &Workspace,
    batch: &srcmv_core::BatchSpecification,
    expected_plan: Option<srcmv_core::Sha256Digest>,
    startup_umask: u32,
    json: bool,
) -> Result<String, (ErrorDto, bool)> {
    let requirements = snapshot_requirements(batch);
    let commit_budget = ResourceBudget::default();
    let prelock_snapshot = workspace
        .acquire_snapshot(&requirements, SnapshotLimits::default())
        .map_err(|error| (filesystem_error(error), json))?;
    let prelock_plan = plan(&prelock_snapshot, batch, commit_budget)
        .map_err(|error| (filesystem_error(error.into()), json))?;
    if expected_plan.is_some_and(|expected| expected != prelock_plan.digest.0) {
        return Err((
            expected_plan_mismatch(expected_plan, prelock_plan.digest.0),
            json,
        ));
    }
    commit_test_hook("after_prelock_plan").map_err(|error| (error, json))?;

    if prelock_plan.usage.changed_targets == 0 {
        let (diagnostic_lock, warnings) =
            diagnostic_context(workspace).map_err(|error| (filesystem_error(error), json))?;
        let response = build_commit_response(
            &prelock_snapshot,
            &prelock_plan,
            workspace,
            warnings,
            None,
            BTreeMap::new(),
        );
        let result = serialize_commit_response(&response, json);
        drop(diagnostic_lock);
        return result;
    }

    let lock = workspace
        .mutation_lock()
        .map_err(|error| (filesystem_error(error), json))?;
    lock.gate_new_transaction()
        .map_err(|error| (filesystem_error(error), json))?;
    let locked_snapshot = workspace
        .acquire_snapshot(&requirements, SnapshotLimits::default())
        .map_err(|error| (filesystem_error(error), json))?;
    let locked_plan = plan(&locked_snapshot, batch, commit_budget)
        .map_err(|error| (filesystem_error(error.into()), json))?;
    if expected_plan.is_some_and(|expected| expected != locked_plan.digest.0) {
        return Err((
            expected_plan_mismatch(expected_plan, locked_plan.digest.0),
            json,
        ));
    }
    if locked_plan.digest != prelock_plan.digest {
        return Err((
            ErrorDto::new(
                ErrorCode::PlanChangedDuringCommit,
                "the resolved plan changed while acquiring the mutation lock",
                BTreeMap::from([
                    (
                        "prelock_plan_sha256".to_owned(),
                        json!(prelock_plan.digest.0.to_prefixed_hex()),
                    ),
                    (
                        "locked_plan_sha256".to_owned(),
                        json!(locked_plan.digest.0.to_prefixed_hex()),
                    ),
                ]),
            ),
            json,
        ));
    }
    let new_file_mode = 0o666 & !startup_umask;
    let outcome = workspace
        .commit(&lock, &locked_snapshot, &locked_plan, new_file_mode)
        .map_err(|error| (filesystem_error(error), json))?;
    let warning = WarningDto::new(
        WarningCode::MetadataNotPreserved,
        "metadata outside content bytes and POSIX permission bits is not preserved",
        BTreeMap::new(),
    );
    let response = build_commit_response(
        &locked_snapshot,
        &locked_plan,
        workspace,
        vec![warning],
        Some(outcome.transaction_id().to_owned()),
        outcome.preserved_permission_modes().clone(),
    );
    serialize_commit_response(&response, json)
}

#[cfg(debug_assertions)]
fn commit_test_hook(name: &str) -> Result<(), ErrorDto> {
    use std::time::{Duration, Instant};

    if std::env::var_os("SRCMV_TEST_HOOK").is_none_or(|value| value != name) {
        return Ok(());
    }
    let ready = std::env::var_os("SRCMV_TEST_READY").ok_or_else(|| {
        ErrorDto::new(
            ErrorCode::InternalError,
            "the commit test hook is missing its ready marker",
            BTreeMap::new(),
        )
    })?;
    let resume = std::env::var_os("SRCMV_TEST_CONTINUE").ok_or_else(|| {
        ErrorDto::new(
            ErrorCode::InternalError,
            "the commit test hook is missing its continue marker",
            BTreeMap::new(),
        )
    })?;
    File::create(&ready).map_err(|_| {
        ErrorDto::new(
            ErrorCode::InternalError,
            "the commit test hook could not publish its ready marker",
            BTreeMap::new(),
        )
    })?;
    let started = Instant::now();
    while !std::path::Path::new(&resume).exists() {
        if started.elapsed() > Duration::from_secs(10) {
            return Err(ErrorDto::new(
                ErrorCode::InternalError,
                "the commit test hook timed out",
                BTreeMap::new(),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
const fn commit_test_hook(_name: &str) -> Result<(), ErrorDto> {
    Ok(())
}

fn expected_plan_mismatch(
    expected: Option<srcmv_core::Sha256Digest>,
    actual: srcmv_core::Sha256Digest,
) -> ErrorDto {
    ErrorDto::new(
        ErrorCode::ExpectedPlanMismatch,
        "the current resolved plan does not match --expect-plan",
        BTreeMap::from([
            (
                "expected_plan_sha256".to_owned(),
                json!(expected.map(srcmv_core::Sha256Digest::to_prefixed_hex)),
            ),
            (
                "actual_plan_sha256".to_owned(),
                json!(actual.to_prefixed_hex()),
            ),
        ]),
    )
}

fn build_commit_response(
    snapshot: &srcmv_core::WorkspaceSnapshot,
    plan: &srcmv_core::EditPlan,
    workspace: &Workspace,
    warnings: Vec<WarningDto>,
    transaction_id: Option<String>,
    preserved_permission_modes: BTreeMap<String, u32>,
) -> CommitResponse {
    let operations = plan
        .operations
        .iter()
        .map(ResolvedOperationResponse::from_committed)
        .collect();
    let outputs = plan
        .outputs
        .iter()
        .map(|output| {
            let before_length = snapshot
                .files
                .iter()
                .find(|file| file.path == output.path)
                .map(|file| u64::try_from(file.bytes.len()).unwrap_or(u64::MAX));
            OutputResponse::new(
                output.path.value.clone(),
                output.change,
                before_length,
                output.original_digest,
                output.resulting_length,
                output.resulting_digest,
            )
        })
        .collect();
    let files_changed = plan
        .outputs
        .iter()
        .filter(|output| output.change != OutputChange::Unchanged)
        .map(|output| output.path.value.clone())
        .collect();
    CommitResponse::new(
        plan.digest.0,
        workspace.identity_hash(),
        operations,
        outputs,
        warnings,
        transaction_id,
        files_changed,
        preserved_permission_modes,
    )
}

fn serialize_commit_response(
    response: &CommitResponse,
    json: bool,
) -> Result<String, (ErrorDto, bool)> {
    if json {
        let line = to_json_line(response).map_err(|error| (error.into_report(), true))?;
        let actual = u64::try_from(line.len()).unwrap_or(u64::MAX);
        enforce_response_bytes(actual).map_err(|error| (error, true))?;
        Ok(line)
    } else {
        let value = serde_json::to_value(response).map_err(|_| {
            (
                ErrorDto::new(
                    ErrorCode::InternalError,
                    "failed to render the commit report",
                    BTreeMap::new(),
                ),
                false,
            )
        })?;
        Ok(format!(
            "plan {}\ntransaction {}\nstate {}\n",
            escape_terminal_text(value["plan_sha256"].as_str().unwrap_or("<invalid>")),
            escape_terminal_text(value["transaction_id"].as_str().unwrap_or("none (no-op)")),
            escape_terminal_text(value["transaction_state"].as_str().unwrap_or("<invalid>"))
        ))
    }
}

fn serialize_preview(
    response: &srcmv_protocol::PreviewResponse,
) -> Result<String, (ErrorDto, bool)> {
    let line = to_json_line(response).map_err(|error| (error.into_report(), true))?;
    let actual = u64::try_from(line.len()).unwrap_or(u64::MAX);
    enforce_response_bytes(actual).map_err(|error| (error, true))?;
    Ok(line)
}

fn enforce_response_bytes(actual: u64) -> Result<(), ErrorDto> {
    if actual > MAX_RESPONSE_BYTES {
        Err(limit_error(
            "serialized_json_response",
            actual,
            MAX_RESPONSE_BYTES,
        ))
    } else {
        Ok(())
    }
}

fn snapshot_requirements(batch: &srcmv_core::BatchSpecification) -> Vec<SnapshotRequirement> {
    let mut requirements = Vec::with_capacity(batch.operations.len().saturating_mul(2));
    for operation in batch.operations.iter() {
        let specification = match operation {
            Operation::Move(specification) | Operation::Copy(specification) => specification,
        };
        requirements.push(SnapshotRequirement {
            path: specification.source.path.clone(),
            state: required_state(&specification.source.precondition),
        });
        requirements.push(SnapshotRequirement {
            path: specification.destination.path.clone(),
            state: required_state(&specification.destination.precondition),
        });
    }
    requirements
}

fn required_state(precondition: &Precondition) -> RequiredPathState {
    match precondition {
        Precondition::Sha256(digest) => RequiredPathState::Existing(*digest),
        Precondition::MustNotExist => RequiredPathState::Absent,
    }
}

fn read_request(path: &str, stdin: &mut dyn Read) -> Result<Vec<u8>, ErrorDto> {
    if path == "-" {
        return read_bounded(stdin, "standard input");
    }

    let mut file = File::open(path).map_err(|error| {
        ErrorDto::new(
            ErrorCode::IoError,
            "failed to open the request file",
            BTreeMap::from([
                ("io_kind".to_string(), json!(format!("{:?}", error.kind()))),
                ("path".to_string(), json!(redact_path(path))),
            ]),
        )
    })?;
    read_bounded(&mut file, "request file")
}

fn read_bounded(reader: &mut dyn Read, source: &'static str) -> Result<Vec<u8>, ErrorDto> {
    let take_limit = MAX_REQUEST_BYTES.saturating_add(1);
    let mut bounded = reader.take(take_limit);
    let mut bytes = Vec::new();
    bounded.read_to_end(&mut bytes).map_err(|error| {
        ErrorDto::new(
            ErrorCode::IoError,
            "failed to read the JSON request",
            BTreeMap::from([
                ("io_kind".to_string(), json!(format!("{:?}", error.kind()))),
                ("source".to_string(), json!(source)),
            ]),
        )
    })?;
    Ok(bytes)
}

fn validate_inspect_paths(paths: &[String]) -> Result<(), ErrorDto> {
    let actual = u64::try_from(paths.len()).unwrap_or(u64::MAX);
    if actual > MAX_OPERATION_PATHS {
        return Err(limit_error("operation_paths", actual, MAX_OPERATION_PATHS));
    }
    for path in paths {
        let length = u64::try_from(path.len()).unwrap_or(u64::MAX);
        if length > MAX_PATH_BYTES {
            return Err(limit_error("path_bytes", length, MAX_PATH_BYTES));
        }
        if path.contains('\0') {
            return Err(ErrorDto::new(
                ErrorCode::InvalidRequest,
                "an inspection path contains an invalid value",
                BTreeMap::from([("reason".to_string(), json!("path_contains_nul"))]),
            ));
        }
    }
    Ok(())
}

fn limit_error(resource: &'static str, actual: u64, limit: u64) -> ErrorDto {
    ErrorDto::new(
        ErrorCode::ResourceLimitExceeded,
        "a command resource limit was exceeded",
        BTreeMap::from([
            ("actual".to_string(), json!(actual)),
            ("limit".to_string(), json!(limit)),
            ("resource".to_string(), json!(resource)),
        ]),
    )
}

fn reject_workspace_for_target_independent(
    has_workspace: bool,
    json: bool,
) -> Result<(), (ErrorDto, bool)> {
    if has_workspace {
        return Err((
            ErrorDto::new(
                ErrorCode::InvalidCli,
                "--workspace is not accepted by target-independent commands",
                BTreeMap::new(),
            ),
            json,
        ));
    }
    Ok(())
}

fn serialize_success<T: serde::Serialize>(
    response: &T,
    json: bool,
) -> Result<String, (ErrorDto, bool)> {
    to_json_line(response).map_err(|error| (error.into_report(), json))
}

fn render_success(response: &str, stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    if stdout.write_all(response.as_bytes()).is_ok() {
        0
    } else {
        let _ = stderr.write_all(b"srcmv: INTERNAL_ERROR: failed to write stdout\n");
        8
    }
}

fn render_error(
    report: &ErrorDto,
    json: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let result = if json {
        to_json_line(report)
            .and_then(|line| {
                stdout.write_all(line.as_bytes()).map_err(|_| {
                    srcmv_protocol::ProtocolError::new(ErrorDto::new(
                        ErrorCode::InternalError,
                        "failed to write stdout",
                        BTreeMap::new(),
                    ))
                })
            })
            .is_ok()
    } else {
        let line = format!(
            "srcmv: {}: {}\n",
            report.code().as_str(),
            escape_terminal_text(report.message())
        );
        stderr.write_all(line.as_bytes()).is_ok()
    };

    if result { report.exit_code() } else { 8 }
}

fn render_selection_error(
    failure: &select::SelectionFailure,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let report = failure.report();
    let written = if failure.json() {
        match srcmv_protocol::to_selection_json_line(report) {
            Ok(line) => stdout.write_all(line.as_bytes()).is_ok(),
            Err(_) => false,
        }
    } else {
        let line = format!(
            "srcmv: {}: {}\n",
            report.code().as_str(),
            escape_terminal_text(report.message())
        );
        stderr.write_all(line.as_bytes()).is_ok()
    };

    if written { report.exit_code() } else { 8 }
}

fn render_outline_error(
    failure: &outline::OutlineFailure,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let report = failure.report();
    let written = if failure.json() {
        match srcmv_protocol::to_outline_json_line(report) {
            Ok(line) => stdout.write_all(line.as_bytes()).is_ok(),
            Err(_) => false,
        }
    } else {
        let line = format!(
            "srcmv: {}: {}\n",
            report.code().as_str(),
            escape_terminal_text(report.message())
        );
        stderr.write_all(line.as_bytes()).is_ok()
    };

    if written { report.exit_code() } else { 8 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoke(arguments: &[&str], stdin: &[u8]) -> (u8, String, String) {
        let mut input = stdin;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run_with_io(arguments, &mut input, &mut stdout, &mut stderr);
        (
            status,
            String::from_utf8(stdout).expect("stdout must be UTF-8"),
            String::from_utf8(stderr).expect("stderr must be UTF-8"),
        )
    }

    #[test]
    fn commit_should_require_one_expected_plan_policy() {
        let request = br#"{"protocol_version":1,"operations":[]}"#;

        let (status, stdout, stderr) = invoke(
            &["srcmv", "apply", "--request", "-", "--commit", "--json"],
            request,
        );

        assert_eq!(status, 3);
        assert!(stdout.contains("EXPECTED_PLAN_REQUIRED"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn human_errors_should_escape_terminal_control_characters() {
        let report = ErrorDto::new(
            ErrorCode::InvalidCli,
            "unsafe\u{202e}\nmessage",
            BTreeMap::new(),
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = render_error(&report, false, &mut stdout, &mut stderr);

        assert_eq!(status, 2);
        assert_eq!(
            stderr,
            b"srcmv: INVALID_CLI: unsafe\\u{202e}\\u{a}message\n"
        );
    }

    #[test]
    fn phase9_serialized_response_limit_covers_below_at_and_above_boundaries() {
        assert!(enforce_response_bytes(MAX_RESPONSE_BYTES - 1).is_ok());
        assert!(enforce_response_bytes(MAX_RESPONSE_BYTES).is_ok());
        let error = enforce_response_bytes(MAX_RESPONSE_BYTES + 1)
            .expect_err("above-limit response should be rejected");
        assert_eq!(error.code(), ErrorCode::ResourceLimitExceeded);
    }
}
