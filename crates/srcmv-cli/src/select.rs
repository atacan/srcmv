//! Semantic source selection command orchestration.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::time::Duration;

use clap::{ArgAction, Args, ValueEnum};
use gen_lsp_types::{Range, WorkspaceFolder};
use serde_json::json;
use sha2::{Digest, Sha256};
use srcmv_core::{FileSnapshot, Sha256Digest, WorkspaceRelativePath};
use srcmv_fs::{
    FsError, MAX_LINE_INDEX_MEMORY, MAX_PATH_BYTES, MAX_TOTAL_LINE_COUNT, SnapshotLimits, Workspace,
};
use srcmv_lsp::capabilities::{CapabilityError, SupportedPositionEncoding};
use srcmv_lsp::config::{
    ConfigError, ResolutionRequest, ResolvedServer, ServerSelection, UserConfiguration,
    load_user_configuration, resolve_server, user_configuration_path,
};
use srcmv_lsp::position::{PositionConverter, PositionError, PositionLimits};
use srcmv_lsp::process::{ProcessError, ProcessFaultKind};
use srcmv_lsp::session::{
    ImmutableDocument, SessionDeadlines, SessionError, SessionInput, SessionLimits, SessionPhase,
    run_session,
};
use srcmv_lsp::symbols::{
    AmbiguityCandidate, KnownSymbolKind, MatchMode, NormalizedSymbolKind, SelectionExtent,
    SymbolError, SymbolLimits, SymbolMatch, normalize_document_symbols, resolve_name,
    resolve_position,
};
use srcmv_lsp::transport::{TransportError, TransportLimits};
use srcmv_protocol::{
    MAX_RESPONSE_BYTES, SelectionByteSelectorDto, SelectionErrorCode, SelectionErrorDto,
    SelectionExtentDto, SelectionKnownSymbolKindDto, SelectionLspPositionDto, SelectionLspRangeDto,
    SelectionMatchDto, SelectionPositionEncodingDto, SelectionQueryDto, SelectionResponse,
    SelectionServerDto, SelectionSourceDto, SelectionSymbolKindDto, WarningDto,
    escape_terminal_text, to_selection_json_line,
};
use url::Url;

use crate::diagnostic_context;

const MAXIMUM_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAXIMUM_SERVER_IDENTITY_BYTES: usize = 1024;
const MAXIMUM_QUERY_NAME_BYTES: usize = 4096;

/// Arguments accepted by `srcmv select`.
#[derive(Debug, Args)]
pub(crate) struct SelectArgs {
    /// Workspace-relative source path.
    #[arg(long, value_name = "RELATIVE", required = true)]
    path: String,
    /// Exact, case-sensitive unqualified symbol name.
    #[arg(
        long,
        value_name = "NAME",
        required_unless_present_any = ["at_byte", "at_line"],
        conflicts_with_all = ["at_byte", "at_line"]
    )]
    name: Option<String>,
    /// Zero-based exact snapshot byte insertion offset.
    #[arg(long, value_name = "OFFSET", conflicts_with = "at_line")]
    at_byte: Option<u64>,
    /// One-based source line containing the desired symbol.
    #[arg(long, value_name = "LINE")]
    at_line: Option<u64>,
    /// One-based Unicode scalar insertion column, defaulting to one.
    #[arg(long, value_name = "COLUMN", requires = "at_line")]
    at_column: Option<u64>,
    /// Optional standardized LSP symbol kind.
    #[arg(long, value_name = "KIND")]
    kind: Option<String>,
    /// Return every bounded match instead of requiring a unique match.
    #[arg(long = "all", action = ArgAction::SetTrue)]
    all_matches: bool,
    /// Byte extent reported for each match.
    #[arg(long, value_enum, default_value_t = ExtentArgument::DeclarationLines)]
    extent: ExtentArgument,
    /// Trusted user or built-in server descriptor ID.
    #[arg(long, value_name = "ID", conflicts_with = "server_program")]
    server_id: Option<String>,
    /// Explicit executable name or path, passed directly without a shell.
    #[arg(
        long,
        value_name = "PROGRAM",
        conflicts_with = "server_id",
        requires = "language_id"
    )]
    server_program: Option<OsString>,
    /// One literal argument passed to an explicit server program.
    #[arg(long, value_name = "ARG", requires = "server_program")]
    server_arg: Vec<String>,
    /// LSP language ID required with an explicit server program.
    #[arg(long, value_name = "ID", requires = "server_program")]
    language_id: Option<String>,
    /// Emit the selection-v1 JSON response.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ExtentArgument {
    Symbol,
    #[value(name = "declaration_lines")]
    DeclarationLines,
}

impl From<ExtentArgument> for SelectionExtent {
    fn from(value: ExtentArgument) -> Self {
        match value {
            ExtentArgument::Symbol => Self::Symbol,
            ExtentArgument::DeclarationLines => Self::DeclarationLines,
        }
    }
}

/// A rendered selection failure and the output mode requested by the caller.
pub(crate) struct SelectionFailure {
    report: SelectionErrorDto,
    json: bool,
}

impl SelectionFailure {
    pub(crate) const fn report(&self) -> &SelectionErrorDto {
        &self.report
    }

    pub(crate) const fn json(&self) -> bool {
        self.json
    }
}

struct ValidatedQuery {
    kind: Option<KnownSymbolKind>,
    value: QueryValue,
}

enum QueryValue {
    Name(String),
    Byte(u64),
}

/// Executes one complete read-only semantic selection.
pub(crate) fn execute(
    workspace_path: Option<&Path>,
    arguments: SelectArgs,
) -> Result<String, SelectionFailure> {
    let json_output = arguments.json;
    execute_inner(workspace_path, arguments).map_err(|report| SelectionFailure {
        report,
        json: json_output,
    })
}

fn execute_inner(
    workspace_path: Option<&Path>,
    arguments: SelectArgs,
) -> Result<String, SelectionErrorDto> {
    let kind = parse_kind(arguments.kind.as_deref())?;
    validate_name(arguments.name.as_deref())?;
    let workspace = Workspace::open(workspace_path.unwrap_or_else(|| Path::new(".")))
        .map_err(map_filesystem_error)?;
    let (diagnostic_lock, warnings) =
        diagnostic_context(&workspace).map_err(map_filesystem_error)?;
    let source_path = WorkspaceRelativePath {
        value: arguments.path.clone(),
    };
    let snapshot = workspace
        .acquire_existing_file(&source_path, selection_snapshot_limits())
        .map_err(map_filesystem_error)?;
    drop(diagnostic_lock);

    let source = std::str::from_utf8(&snapshot.bytes).map_err(|_| {
        error(
            SelectionErrorCode::UnsupportedTextEncoding,
            "the source snapshot is not valid UTF-8",
        )
    })?;
    let query = validate_query(&arguments, kind, source, &snapshot)?;
    let configuration = load_optional_configuration()?;
    let resolved =
        resolve_selection_server(&workspace, &snapshot, &arguments, configuration.as_ref())?;
    let project_uri = Url::from_directory_path(&resolved.project_root)
        .map_err(|()| internal_error("failed to construct the project-root file URI"))?;
    let source_uri = Url::from_file_path(workspace.canonical_root().join(&snapshot.path.value))
        .map_err(|()| internal_error("failed to construct the source file URI"))?;
    let workspace_name = resolved
        .project_root
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("workspace")
        .to_owned();
    let document = ImmutableDocument {
        uri: source_uri,
        language_id: resolved.language_id.clone(),
        text: source.to_owned(),
    };
    let deadlines = session_deadlines(&resolved);
    let input = SessionInput {
        process: resolved.process.clone(),
        workspace: WorkspaceFolder::new(project_uri, workspace_name),
        document,
        initialization_options: resolved.initialization_options.clone(),
        settings: resolved.settings.clone(),
        deadlines,
        limits: SessionLimits::default(),
    };
    let output = run_session(input, TransportLimits::default()).map_err(map_session_error)?;
    validate_server_identity(&output.capabilities.server.name, "reported server name")?;
    validate_server_identity(
        &output.capabilities.server.version,
        "reported server version",
    )?;

    let mut converter = PositionConverter::new(
        source,
        &snapshot.line_index,
        output.capabilities.position_encoding,
        PositionLimits::default(),
    )
    .map_err(map_position_protocol_error)?;
    let symbols =
        normalize_document_symbols(output.symbols, &mut converter, SymbolLimits::default())
            .map_err(map_symbol_error)?;
    let extent = SelectionExtent::from(arguments.extent);
    let mode = if arguments.all_matches {
        MatchMode::All
    } else {
        MatchMode::Unique
    };
    let matches = match &query.value {
        QueryValue::Name(name) => resolve_name(
            &symbols,
            source,
            name,
            query.kind,
            extent,
            mode,
            SymbolLimits::default(),
        ),
        QueryValue::Byte(byte_offset) => resolve_position(
            &symbols,
            source,
            *byte_offset,
            query.kind,
            extent,
            mode,
            SymbolLimits::default(),
        ),
    }
    .map_err(map_symbol_error)?;

    let response = build_response(
        &workspace,
        &snapshot,
        &query,
        &resolved,
        output.capabilities.position_encoding,
        output.capabilities.server.name,
        output.capabilities.server.version,
        &matches,
        warnings,
    )?;
    if arguments.json {
        to_selection_json_line(&response)
            .map_err(srcmv_protocol::SelectionProtocolError::into_report)
    } else {
        render_human(&snapshot.path.value, &matches)
    }
}

pub(crate) fn selection_snapshot_limits() -> SnapshotLimits {
    SnapshotLimits::new(
        MAX_PATH_BYTES,
        1,
        MAXIMUM_SOURCE_BYTES,
        MAXIMUM_SOURCE_BYTES,
        MAX_TOTAL_LINE_COUNT,
        MAX_LINE_INDEX_MEMORY,
    )
}
fn validate_query(
    arguments: &SelectArgs,
    kind: Option<KnownSymbolKind>,
    source: &str,
    snapshot: &FileSnapshot,
) -> Result<ValidatedQuery, SelectionErrorDto> {
    let mut converter = PositionConverter::new(
        source,
        &snapshot.line_index,
        SupportedPositionEncoding::Utf8,
        PositionLimits::default(),
    )
    .map_err(map_user_position_error)?;
    let value = if let Some(name) = arguments.name.as_ref() {
        QueryValue::Name(name.clone())
    } else if let Some(byte_offset) = arguments.at_byte {
        QueryValue::Byte(
            converter
                .validate_user_byte(byte_offset)
                .map_err(map_user_position_error)?,
        )
    } else if let Some(line) = arguments.at_line {
        QueryValue::Byte(
            converter
                .user_line_scalar_to_byte(line, arguments.at_column.unwrap_or(1))
                .map_err(map_user_position_error)?,
        )
    } else {
        return Err(error(
            SelectionErrorCode::InvalidSelectionQuery,
            "exactly one semantic-selection query is required",
        ));
    };
    Ok(ValidatedQuery { kind, value })
}

fn validate_name(name: Option<&str>) -> Result<(), SelectionErrorDto> {
    if name.is_some_and(str::is_empty) {
        Err(error(
            SelectionErrorCode::InvalidSelectionQuery,
            "a symbol name must not be empty",
        ))
    } else if name.is_some_and(|name| name.len() > MAXIMUM_QUERY_NAME_BYTES) {
        Err(resource_error(
            "selection query name bytes",
            MAXIMUM_QUERY_NAME_BYTES,
        ))
    } else {
        Ok(())
    }
}

fn parse_kind(value: Option<&str>) -> Result<Option<KnownSymbolKind>, SelectionErrorDto> {
    value.map(str::parse).transpose().map_err(|_| {
        error(
            SelectionErrorCode::InvalidSelectionQuery,
            "the symbol kind is not recognized",
        )
    })
}

pub(crate) fn load_optional_configuration() -> Result<Option<UserConfiguration>, SelectionErrorDto>
{
    let explicit = env::var_os(srcmv_lsp::config::CONFIGURATION_PATH_ENVIRONMENT_VARIABLE);
    let Some(path) = user_configuration_path(explicit.as_deref()) else {
        return Ok(None);
    };
    match load_user_configuration(&path) {
        Ok(configuration) => Ok(Some(configuration)),
        Err(ConfigError::Read {
            kind: io::ErrorKind::NotFound,
        }) if explicit.is_none() => Ok(None),
        Err(error) => Err(map_config_error(error)),
    }
}

pub(crate) fn resolve_selection_server(
    workspace: &Workspace,
    snapshot: &FileSnapshot,
    arguments: &SelectArgs,
    configuration: Option<&UserConfiguration>,
) -> Result<ResolvedServer, SelectionErrorDto> {
    let selection = if let Some(program) = arguments.server_program.as_deref() {
        ServerSelection::Program {
            program,
            arguments: &arguments.server_arg,
            language_id: arguments.language_id.as_deref().unwrap_or(""),
        }
    } else if let Some(id) = arguments.server_id.as_deref() {
        ServerSelection::Id(id)
    } else {
        ServerSelection::Automatic
    };
    resolve_server_for_source(workspace, snapshot, selection, configuration)
}

/// Resolves one trusted server descriptor for an already acquired snapshot.
///
/// Shared by every read-only language-server surface; the caller supplies its
/// own explicit-program, identifier, or automatic selection policy.
pub(crate) fn resolve_server_for_source(
    workspace: &Workspace,
    snapshot: &FileSnapshot,
    selection: ServerSelection<'_>,
    configuration: Option<&UserConfiguration>,
) -> Result<ResolvedServer, SelectionErrorDto> {
    let extension = Path::new(&snapshot.path.value)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("");
    resolve_server(
        configuration,
        ResolutionRequest {
            workspace_root: workspace.canonical_root(),
            source_extension: extension,
            selection,
            executable_path: env::var_os("PATH").as_deref(),
        },
    )
    .map_err(map_config_error)
}

pub(crate) fn session_deadlines(server: &ResolvedServer) -> SessionDeadlines {
    let shutdown = Duration::from_secs(5);
    let cleanup = Duration::from_secs(5);
    let scheduling_allowance = Duration::from_secs(10);
    SessionDeadlines {
        initialize: server.startup_timeout,
        document_symbols: server.request_timeout,
        shutdown,
        cleanup,
        total: server
            .startup_timeout
            .saturating_add(server.request_timeout)
            .saturating_add(shutdown)
            .saturating_add(cleanup)
            .saturating_add(scheduling_allowance),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "selection wire inputs are explicit"
)]
fn build_response(
    workspace: &Workspace,
    snapshot: &FileSnapshot,
    query: &ValidatedQuery,
    server: &ResolvedServer,
    position_encoding: SupportedPositionEncoding,
    reported_name: Option<String>,
    reported_version: Option<String>,
    matches: &[SymbolMatch],
    warnings: Vec<WarningDto>,
) -> Result<SelectionResponse, SelectionErrorDto> {
    let query = match &query.value {
        QueryValue::Name(name) => {
            SelectionQueryDto::name(name, query.kind.map(protocol_known_kind))
        }
        QueryValue::Byte(offset) => {
            SelectionQueryDto::position(*offset, query.kind.map(protocol_known_kind))
        }
    };
    let workspace_identity_hash = workspace.identity_hash();
    let source = SelectionSourceDto::new(
        snapshot.path.value.clone(),
        snapshot.digest,
        u64::try_from(snapshot.bytes.len()).unwrap_or(u64::MAX),
    );
    let server = SelectionServerDto::new(
        server.configuration_id.clone(),
        reported_name,
        reported_version,
        protocol_position_encoding(position_encoding),
    );
    let empty_response = SelectionResponse::new(
        workspace_identity_hash,
        source.clone(),
        query.clone(),
        server.clone(),
        Vec::new(),
        warnings.clone(),
    );
    let mut exact_response_bytes = serde_json::to_vec(&empty_response)
        .map_err(|_| internal_error("failed to measure the selection response"))?
        .len()
        .checked_add(1)
        .ok_or_else(response_size_error)?;
    let maximum = usize::try_from(MAX_RESPONSE_BYTES).unwrap_or(usize::MAX);
    if exact_response_bytes > maximum {
        return Err(response_size_error());
    }
    let mut response_matches = Vec::with_capacity(matches.len());
    for selected in matches {
        let response_match = build_match(snapshot, selected)?;
        let match_bytes = serde_json::to_vec(&response_match)
            .map_err(|_| internal_error("failed to measure a selection match"))?
            .len();
        exact_response_bytes = exact_response_bytes
            .checked_add(match_bytes)
            .and_then(|bytes| bytes.checked_add(usize::from(!response_matches.is_empty())))
            .ok_or_else(response_size_error)?;
        if exact_response_bytes > maximum {
            return Err(response_size_error());
        }
        response_matches.push(response_match);
    }
    Ok(SelectionResponse::new(
        workspace_identity_hash,
        source,
        query,
        server,
        response_matches,
        warnings,
    ))
}

fn build_match(
    snapshot: &FileSnapshot,
    selected: &SymbolMatch,
) -> Result<SelectionMatchDto, SelectionErrorDto> {
    let start = usize::try_from(selected.selected_range.start)
        .map_err(|_| internal_error("selected range does not fit memory"))?;
    let end = usize::try_from(selected.selected_range.end)
        .map_err(|_| internal_error("selected range does not fit memory"))?;
    let payload = snapshot
        .bytes
        .get(start..end)
        .ok_or_else(|| internal_error("selected range is outside the immutable snapshot"))?;
    let selected_digest = Sha256Digest(Sha256::digest(payload).into());
    let selector =
        SelectionByteSelectorDto::new(selected.selected_range.start, selected.selected_range.end);
    Ok(SelectionMatchDto::new(
        selected.name.clone(),
        protocol_symbol_kind(selected.kind),
        selected.symbol_path.clone(),
        selected.detail.clone(),
        protocol_lsp_range(selected.lsp_range),
        protocol_lsp_range(selected.lsp_selection_range),
        match selected.extent {
            SelectionExtent::Symbol => SelectionExtentDto::Symbol,
            SelectionExtent::DeclarationLines => SelectionExtentDto::DeclarationLines,
        },
        selector,
        selected_digest,
        snapshot.path.value.clone(),
        snapshot.digest,
    ))
}

pub(crate) fn protocol_lsp_range(range: Range) -> SelectionLspRangeDto {
    SelectionLspRangeDto::new(
        SelectionLspPositionDto::new(range.start.line, range.start.character),
        SelectionLspPositionDto::new(range.end.line, range.end.character),
    )
}

pub(crate) fn protocol_position_encoding(
    value: SupportedPositionEncoding,
) -> SelectionPositionEncodingDto {
    match value {
        SupportedPositionEncoding::Utf8 => SelectionPositionEncodingDto::Utf8,
        SupportedPositionEncoding::Utf16 => SelectionPositionEncodingDto::Utf16,
        SupportedPositionEncoding::Utf32 => SelectionPositionEncodingDto::Utf32,
    }
}

pub(crate) fn protocol_symbol_kind(value: NormalizedSymbolKind) -> SelectionSymbolKindDto {
    match value {
        NormalizedSymbolKind::Known(kind) => protocol_known_kind(kind).into(),
        NormalizedSymbolKind::Unknown(_) => SelectionSymbolKindDto::Unknown,
    }
}

fn protocol_known_kind(value: KnownSymbolKind) -> SelectionKnownSymbolKindDto {
    use KnownSymbolKind as K;
    match value {
        K::File => SelectionKnownSymbolKindDto::File,
        K::Module => SelectionKnownSymbolKindDto::Module,
        K::Namespace => SelectionKnownSymbolKindDto::Namespace,
        K::Package => SelectionKnownSymbolKindDto::Package,
        K::Class => SelectionKnownSymbolKindDto::Class,
        K::Method => SelectionKnownSymbolKindDto::Method,
        K::Property => SelectionKnownSymbolKindDto::Property,
        K::Field => SelectionKnownSymbolKindDto::Field,
        K::Constructor => SelectionKnownSymbolKindDto::Constructor,
        K::Enum => SelectionKnownSymbolKindDto::Enum,
        K::Interface => SelectionKnownSymbolKindDto::Interface,
        K::Function => SelectionKnownSymbolKindDto::Function,
        K::Variable => SelectionKnownSymbolKindDto::Variable,
        K::Constant => SelectionKnownSymbolKindDto::Constant,
        K::String => SelectionKnownSymbolKindDto::String,
        K::Number => SelectionKnownSymbolKindDto::Number,
        K::Boolean => SelectionKnownSymbolKindDto::Boolean,
        K::Array => SelectionKnownSymbolKindDto::Array,
        K::Object => SelectionKnownSymbolKindDto::Object,
        K::Key => SelectionKnownSymbolKindDto::Key,
        K::Null => SelectionKnownSymbolKindDto::Null,
        K::EnumMember => SelectionKnownSymbolKindDto::EnumMember,
        K::Struct => SelectionKnownSymbolKindDto::Struct,
        K::Event => SelectionKnownSymbolKindDto::Event,
        K::Operator => SelectionKnownSymbolKindDto::Operator,
        K::TypeParameter => SelectionKnownSymbolKindDto::TypeParameter,
    }
}

fn render_human(path: &str, matches: &[SymbolMatch]) -> Result<String, SelectionErrorDto> {
    let maximum = usize::try_from(MAX_RESPONSE_BYTES).unwrap_or(usize::MAX);
    let mut output = String::new();
    for selected in matches {
        let range = selected.lsp_range;
        let selection = selected.lsp_selection_range;
        let line = format!(
            "{}:{}..{} {} {} [{}] lsp={}:{}..{}:{} selection={}:{}..{}:{}\n",
            escape_terminal_text(path),
            selected.selected_range.start,
            selected.selected_range.end,
            selected.kind.as_str(),
            escape_terminal_text(&selected.name),
            escape_terminal_text(&selected.symbol_path.join("::")),
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
            selection.start.line,
            selection.start.character,
            selection.end.line,
            selection.end.character,
        );
        let next_size = output
            .len()
            .checked_add(line.len())
            .ok_or_else(response_size_error)?;
        if next_size > maximum {
            return Err(response_size_error());
        }
        output.push_str(&line);
    }
    if matches.is_empty() {
        output.push_str("no matching document symbols\n");
    }
    Ok(output)
}

fn response_size_error() -> SelectionErrorDto {
    resource_error(
        "serialized selection response",
        usize::try_from(MAX_RESPONSE_BYTES).unwrap_or(usize::MAX),
    )
}

pub(crate) fn validate_server_identity(
    value: &Option<String>,
    resource: &'static str,
) -> Result<(), SelectionErrorDto> {
    if value.as_ref().is_some_and(String::is_empty) {
        Err(error(
            SelectionErrorCode::LspProtocolError,
            "the language server reported an empty identity field",
        ))
    } else if value
        .as_ref()
        .is_some_and(|value| value.len() > MAXIMUM_SERVER_IDENTITY_BYTES)
    {
        Err(resource_error(resource, MAXIMUM_SERVER_IDENTITY_BYTES))
    } else {
        Ok(())
    }
}

pub(crate) fn map_config_error(error_value: ConfigError) -> SelectionErrorDto {
    let resource = matches!(
        error_value,
        ConfigError::ConfigurationTooLarge { .. }
            | ConfigError::ConfigurationTooDeep { .. }
            | ConfigError::FieldTooLarge { .. }
            | ConfigError::InvalidArguments
            | ConfigError::OversizedJsonConfiguration { .. }
    );
    let invalid_explicit = matches!(error_value, ConfigError::InvalidExplicitSelection);
    if resource {
        error(
            SelectionErrorCode::LspResourceLimitExceeded,
            "the language-server configuration exceeds a resource limit",
        )
    } else if invalid_explicit {
        error(
            SelectionErrorCode::InvalidSelectionQuery,
            "the explicit language-server selection is invalid",
        )
    } else {
        error(
            SelectionErrorCode::LspServerNotConfigured,
            "no trusted language server could be resolved for the source",
        )
    }
}

pub(crate) fn map_filesystem_error(error_value: FsError) -> SelectionErrorDto {
    match error_value {
        FsError::ResourceLimitExceeded {
            resource, limit, ..
        } => resource_error(resource, usize::try_from(limit).unwrap_or(usize::MAX)),
        FsError::InvalidPath { .. }
        | FsError::PathNotFound { .. }
        | FsError::SymlinkNotAllowed { .. }
        | FsError::UnsupportedFileType { .. } => error(
            SelectionErrorCode::InvalidSelectionQuery,
            "the selected source path is not a readable workspace file",
        ),
        _ => internal_error("the workspace could not be inspected safely"),
    }
}

pub(crate) fn map_session_error(error_value: SessionError) -> SelectionErrorDto {
    match error_value {
        SessionError::Capability(CapabilityError::DocumentSymbolsUnavailable) => error(
            SelectionErrorCode::LspCapabilityUnavailable,
            "the language server does not provide document symbols",
        ),
        SessionError::Capability(CapabilityError::DocumentSyncUnavailable) => error(
            SelectionErrorCode::LspDocumentSyncUnavailable,
            "the language server cannot synchronize the immutable source snapshot",
        ),
        SessionError::Capability(CapabilityError::UnsupportedPositionEncoding) => error(
            SelectionErrorCode::LspCapabilityUnavailable,
            "the language server selected an unsupported position encoding",
        ),
        SessionError::Timeout(phase) => timeout_error(phase),
        SessionError::RequestFailed { .. } => error(
            SelectionErrorCode::LspRequestFailed,
            "the language server rejected a required request",
        ),
        SessionError::ResourceLimit { resource, limit } => resource_error(resource, limit),
        SessionError::Transport(transport) => map_transport_error(transport),
        SessionError::InvalidLspPayload(_) | SessionError::UnexpectedResponse => error(
            SelectionErrorCode::LspProtocolError,
            "the language server returned an invalid protocol response",
        ),
    }
}

pub(crate) fn map_transport_error(error_value: TransportError) -> SelectionErrorDto {
    match error_value {
        TransportError::Process(ProcessError::Spawn(_) | ProcessError::SpawnWorker(_)) => error(
            SelectionErrorCode::LspStartFailed,
            "the configured language server could not be started",
        ),
        TransportError::Exited(_) | TransportError::StdoutClosed | TransportError::StdinClosed => {
            error(
                SelectionErrorCode::LspExited,
                "the language server exited before selection completed",
            )
        }
        TransportError::DeadlineExceeded
        | TransportError::Process(ProcessError::DeadlineExceeded(_)) => error(
            SelectionErrorCode::LspTimeout,
            "the language server did not respond before the deadline",
        ),
        TransportError::ResourceLimit { resource, limit } => resource_error(resource, limit),
        TransportError::Process(ProcessError::ByteCapacityExceeded { capacity_bytes, .. }) => {
            resource_error("language-server queued bytes", capacity_bytes)
        }
        TransportError::ProcessFault(fault) => match fault.kind {
            ProcessFaultKind::ResourceLimit { capacity_bytes, .. } => {
                resource_error("language-server queued bytes", capacity_bytes)
            }
            ProcessFaultKind::Io { .. } => error(
                SelectionErrorCode::LspProtocolError,
                "the language-server transport failed",
            ),
        },
        TransportError::Protocol(_) | TransportError::Process(_) => error(
            SelectionErrorCode::LspProtocolError,
            "the language-server transport failed",
        ),
    }
}

pub(crate) fn map_symbol_error(error_value: SymbolError) -> SelectionErrorDto {
    match error_value {
        SymbolError::FlatSymbolsUnsupported => error(
            SelectionErrorCode::LspFlatSymbolsUnsupported,
            "the language server returned unsupported flat document symbols",
        ),
        SymbolError::ResourceLimitExceeded { resource, maximum } => {
            resource_error(resource, usize::try_from(maximum).unwrap_or(usize::MAX))
        }
        SymbolError::NotFound => error(
            SelectionErrorCode::SelectionNotFound,
            "no document symbol matched the selection query",
        ),
        SymbolError::Ambiguous { total, candidates } => ambiguity_error(total, &candidates),
        SymbolError::QueryPositionOutOfBounds => error(
            SelectionErrorCode::InvalidSelectionQuery,
            "the selection position is outside the source snapshot",
        ),
        SymbolError::MalformedDocumentSymbols
        | SymbolError::Position(_)
        | SymbolError::SelectionRangeNotContained
        | SymbolError::InvalidExtent => error(
            SelectionErrorCode::LspProtocolError,
            "the language server returned invalid document-symbol ranges",
        ),
    }
}

fn ambiguity_error(total: u64, candidates: &[AmbiguityCandidate]) -> SelectionErrorDto {
    let candidates = candidates
        .iter()
        .map(|candidate| {
            json!({
                "name": candidate.name,
                "symbol_kind": candidate.kind.as_str(),
                "symbol_path": candidate.symbol_path,
                "selector": {
                    "kind": "bytes",
                    "start": candidate.byte_range.start,
                    "end": candidate.byte_range.end,
                }
            })
        })
        .collect::<Vec<_>>();
    SelectionErrorDto::new(
        SelectionErrorCode::SelectionAmbiguous,
        "the selection query matched more than one document symbol",
        BTreeMap::from([
            ("candidate_count".to_owned(), json!(total)),
            ("candidates".to_owned(), json!(candidates)),
        ]),
    )
}

fn map_user_position_error(_error: PositionError) -> SelectionErrorDto {
    error(
        SelectionErrorCode::InvalidSelectionQuery,
        "the selection position is outside the source snapshot",
    )
}

fn map_position_protocol_error(_error: PositionError) -> SelectionErrorDto {
    error(
        SelectionErrorCode::LspProtocolError,
        "the immutable source position index is invalid",
    )
}

fn timeout_error(phase: SessionPhase) -> SelectionErrorDto {
    let phase = match phase {
        SessionPhase::Initialize => "initialize",
        SessionPhase::DocumentSymbols => "document_symbol",
        SessionPhase::Shutdown => "shutdown",
    };
    SelectionErrorDto::new(
        SelectionErrorCode::LspTimeout,
        "the language server did not respond before the selection deadline",
        BTreeMap::from([("phase".to_owned(), json!(phase))]),
    )
}

fn resource_error(resource: &'static str, limit: usize) -> SelectionErrorDto {
    SelectionErrorDto::new(
        SelectionErrorCode::LspResourceLimitExceeded,
        "semantic selection exceeded a configured resource limit",
        BTreeMap::from([
            ("limit".to_owned(), json!(limit)),
            ("resource".to_owned(), json!(resource)),
        ]),
    )
}

fn internal_error(message: &'static str) -> SelectionErrorDto {
    error(SelectionErrorCode::SelectionInternalError, message)
}

fn error(code: SelectionErrorCode, message: &'static str) -> SelectionErrorDto {
    SelectionErrorDto::new(code, message, BTreeMap::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use srcmv_lsp::process::{ProcessFault, ProcessWorker};

    #[test]
    fn server_identity_must_be_nonempty_and_bounded_for_the_wire_schema() {
        assert!(validate_server_identity(&None, "identity").is_ok());
        assert!(validate_server_identity(&Some("server".to_owned()), "identity").is_ok());
        assert!(
            validate_server_identity(&Some("x".repeat(MAXIMUM_SERVER_IDENTITY_BYTES)), "identity",)
                .is_ok()
        );

        let empty = validate_server_identity(&Some(String::new()), "identity")
            .expect_err("empty identity must fail");
        let oversized = validate_server_identity(
            &Some("x".repeat(MAXIMUM_SERVER_IDENTITY_BYTES + 1)),
            "identity",
        )
        .expect_err("oversized identity must fail");

        assert_eq!(empty.code(), SelectionErrorCode::LspProtocolError);
        assert_eq!(
            oversized.code(),
            SelectionErrorCode::LspResourceLimitExceeded
        );
    }

    #[test]
    fn query_name_bytes_are_checked_below_at_and_above_the_limit() {
        assert!(validate_name(Some(&"x".repeat(MAXIMUM_QUERY_NAME_BYTES - 1))).is_ok());
        assert!(validate_name(Some(&"x".repeat(MAXIMUM_QUERY_NAME_BYTES))).is_ok());

        let oversized = validate_name(Some(&"x".repeat(MAXIMUM_QUERY_NAME_BYTES + 1)))
            .expect_err("a name above the byte limit must fail");
        assert_eq!(
            oversized.code(),
            SelectionErrorCode::LspResourceLimitExceeded
        );
    }

    #[test]
    fn asynchronous_queue_overflow_maps_to_the_resource_error_registry() {
        let report = map_transport_error(TransportError::ProcessFault(ProcessFault {
            worker: ProcessWorker::Stdout,
            kind: ProcessFaultKind::ResourceLimit {
                queue: "stdout",
                item_bytes: 9,
                capacity_bytes: 8,
            },
        }));

        assert_eq!(report.code(), SelectionErrorCode::LspResourceLimitExceeded);
    }

    #[test]
    fn terminal_pipe_closures_map_to_lsp_exited() {
        for error_value in [TransportError::StdinClosed, TransportError::StdoutClosed] {
            let report = map_transport_error(error_value);
            assert_eq!(report.code(), SelectionErrorCode::LspExited);
        }
    }

    #[test]
    fn unrelated_io_fault_remains_a_protocol_error() {
        let report = map_transport_error(TransportError::ProcessFault(ProcessFault {
            worker: ProcessWorker::Stdout,
            kind: ProcessFaultKind::Io {
                error_kind: io::ErrorKind::PermissionDenied,
                message: "fixture I/O failure".to_owned(),
            },
        }));

        assert_eq!(report.code(), SelectionErrorCode::LspProtocolError);
    }
}
