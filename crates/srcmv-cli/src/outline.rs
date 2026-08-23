//! Read-only file outline command orchestration.
//!
//! One `textDocument/documentSymbol` request runs over the shared session
//! lifecycle; the hierarchical result is validated by the existing symbol
//! layer, ordered deterministically, and emitted as flat outline-v1 records.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use clap::Args;
use gen_lsp_types::WorkspaceFolder;
use serde_json::json;
use srcmv_core::WorkspaceRelativePath;
use srcmv_fs::{FsError, Workspace};
use srcmv_lsp::config::ServerSelection;
use srcmv_lsp::position::{PositionConverter, PositionLimits};
use srcmv_lsp::session::{
    ImmutableDocument, SessionError, SessionInput, SessionLimits, run_session,
};
use srcmv_lsp::symbols::{
    DEFAULT_MAXIMUM_OUTLINE_SYMBOLS, KnownSymbolKind, NormalizedSymbol, NormalizedSymbolKind,
    SymbolError, SymbolLimits, normalize_document_symbols, order_unique_candidates,
};
use srcmv_lsp::transport::TransportLimits;
use srcmv_protocol::{
    MAX_RESPONSE_BYTES, OutlineErrorCode, OutlineErrorDto, OutlineProtocolError, OutlineResponse,
    OutlineSymbolDto, SelectionByteSelectorDto, SelectionErrorCode, SelectionErrorDto,
    SelectionServerDto, SelectionSourceDto, escape_terminal_text, to_outline_json_line,
};
use url::Url;

use crate::diagnostic_context;
use crate::select::{
    load_optional_configuration, map_filesystem_error, map_session_error, map_symbol_error,
    protocol_lsp_range, protocol_position_encoding, protocol_symbol_kind,
    resolve_server_for_source, selection_snapshot_limits, session_deadlines,
    validate_server_identity,
};

const KIND_NOT_RECOGNIZED: &str = "the symbol kind is not recognized";
const START_LINE_INVARIANT: &str = "the symbol start offset has no physical line";
const END_LINE_INVARIANT: &str = "the symbol end offset has no physical line";
const START_COLUMN_INVARIANT: &str = "the symbol start offset has no scalar column";

/// Arguments accepted by `srcmv outline`.
#[derive(Debug, Args)]
pub(crate) struct OutlineArgs {
    /// Workspace-relative source path.
    #[arg(long, value_name = "RELATIVE", required = true)]
    path: String,
    /// Optional standardized symbol-kind filter; repeatable.
    #[arg(long = "kind", value_name = "KIND")]
    kinds: Vec<String>,
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
    /// Emit the outline-v1 JSON response.
    #[arg(long)]
    json: bool,
}

/// A rendered outline failure and the output mode requested by the caller.
pub(crate) struct OutlineFailure {
    report: OutlineErrorDto,
    json: bool,
}

impl OutlineFailure {
    pub(crate) const fn report(&self) -> &OutlineErrorDto {
        &self.report
    }

    pub(crate) const fn json(&self) -> bool {
        self.json
    }
}

/// Executes one complete read-only document outline.
pub(crate) fn execute(
    workspace_path: Option<&Path>,
    arguments: OutlineArgs,
) -> Result<String, OutlineFailure> {
    let json_output = arguments.json;
    execute_inner(workspace_path, arguments).map_err(|report| OutlineFailure {
        report,
        json: json_output,
    })
}

fn execute_inner(
    workspace_path: Option<&Path>,
    arguments: OutlineArgs,
) -> Result<String, OutlineErrorDto> {
    let requested_kinds = parse_kinds(&arguments.kinds)?;
    let workspace = Workspace::open(workspace_path.unwrap_or_else(|| Path::new(".")))
        .map_err(outline_filesystem_error)?;
    let (diagnostic_lock, warnings) =
        diagnostic_context(&workspace).map_err(outline_filesystem_error)?;
    let source_path = WorkspaceRelativePath {
        value: arguments.path.clone(),
    };
    let snapshot = workspace
        .acquire_existing_file(&source_path, selection_snapshot_limits())
        .map_err(outline_filesystem_error)?;
    drop(diagnostic_lock);

    let source = std::str::from_utf8(&snapshot.bytes).map_err(|_| {
        OutlineErrorDto::new(
            OutlineErrorCode::UnsupportedTextEncoding,
            "the source snapshot is not valid UTF-8",
            BTreeMap::new(),
        )
    })?;
    let configuration = load_optional_configuration().map_err(retarget_report)?;
    let resolved = resolve_server_for_source(
        &workspace,
        &snapshot,
        server_selection(&arguments),
        configuration.as_ref(),
    )
    .map_err(retarget_report)?;
    let project_uri = Url::from_directory_path(&resolved.project_root)
        .map_err(|()| outline_internal_error("failed to construct the project-root file URI"))?;
    let source_uri = Url::from_file_path(workspace.canonical_root().join(&snapshot.path.value))
        .map_err(|()| outline_internal_error("failed to construct the source file URI"))?;
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
    let input = SessionInput {
        process: resolved.process.clone(),
        workspace: WorkspaceFolder::new(project_uri, workspace_name),
        document,
        initialization_options: resolved.initialization_options.clone(),
        settings: resolved.settings.clone(),
        deadlines: session_deadlines(&resolved),
        limits: SessionLimits::default(),
    };
    let output = run_session(input, TransportLimits::default()).map_err(outline_session_error)?;
    validate_server_identity(&output.capabilities.server.name, "reported server name")
        .map_err(retarget_report)?;
    validate_server_identity(
        &output.capabilities.server.version,
        "reported server version",
    )
    .map_err(retarget_report)?;

    let mut converter = PositionConverter::new(
        source,
        &snapshot.line_index,
        output.capabilities.position_encoding,
        PositionLimits::default(),
    )
    .map_err(|_| outline_protocol_error())?;
    let normalized =
        normalize_document_symbols(output.symbols, &mut converter, SymbolLimits::default())
            .map_err(outline_symbol_error)?;
    let ordered = order_unique_candidates(&normalized);
    let filtered = filter_kinds(&ordered, &requested_kinds);
    enforce_outline_limit(filtered.len())?;
    let symbols: Vec<_> = filtered
        .iter()
        .map(|candidate| outline_symbol(candidate, &mut converter))
        .collect::<Result<_, _>>()?;

    if arguments.json {
        let response = OutlineResponse::new(
            workspace.identity_hash(),
            SelectionSourceDto::new(
                snapshot.path.value.clone(),
                snapshot.digest,
                u64::try_from(snapshot.bytes.len()).unwrap_or(u64::MAX),
            ),
            SelectionServerDto::new(
                resolved.configuration_id.clone(),
                output.capabilities.server.name,
                output.capabilities.server.version,
                protocol_position_encoding(output.capabilities.position_encoding),
            ),
            symbols,
            warnings,
        );
        to_outline_json_line(&response).map_err(OutlineProtocolError::into_report)
    } else {
        render_human(&snapshot.path.value, &symbols)
    }
}

fn server_selection(arguments: &OutlineArgs) -> ServerSelection<'_> {
    if let Some(program) = arguments.server_program.as_deref() {
        ServerSelection::Program {
            program,
            arguments: &arguments.server_arg,
            language_id: arguments.language_id.as_deref().unwrap_or(""),
        }
    } else if let Some(id) = arguments.server_id.as_deref() {
        ServerSelection::Id(id)
    } else {
        ServerSelection::Automatic
    }
}

fn parse_kinds(values: &[String]) -> Result<Vec<KnownSymbolKind>, OutlineErrorDto> {
    values
        .iter()
        .map(|value| {
            value.parse().map_err(|_| {
                OutlineErrorDto::new(
                    OutlineErrorCode::InvalidOutlineQuery,
                    KIND_NOT_RECOGNIZED,
                    BTreeMap::new(),
                )
            })
        })
        .collect()
}

fn filter_kinds<'a>(
    candidates: &[&'a NormalizedSymbol],
    requested: &[KnownSymbolKind],
) -> Vec<&'a NormalizedSymbol> {
    candidates
        .iter()
        .copied()
        .filter(|candidate| {
            requested.is_empty()
                || requested
                    .iter()
                    .any(|kind| candidate.kind == NormalizedSymbolKind::Known(*kind))
        })
        .collect()
}

fn enforce_outline_limit(count: usize) -> Result<(), OutlineErrorDto> {
    if count > DEFAULT_MAXIMUM_OUTLINE_SYMBOLS {
        return Err(resource_limit_error(
            "outline_symbols",
            DEFAULT_MAXIMUM_OUTLINE_SYMBOLS,
        ));
    }
    Ok(())
}

fn outline_symbol(
    candidate: &NormalizedSymbol,
    converter: &mut PositionConverter<'_>,
) -> Result<OutlineSymbolDto, OutlineErrorDto> {
    let (start_line, start_column) = converter
        .byte_to_user_line_scalar(candidate.byte_range.start)
        .map_err(|_| outline_internal_error(START_LINE_INVARIANT))?;
    let Some(start_column) = start_column else {
        return Err(outline_internal_error(START_COLUMN_INVARIANT));
    };
    let (end_line, end_column) = converter
        .byte_to_user_line_scalar(candidate.byte_range.end)
        .map_err(|_| outline_internal_error(END_LINE_INVARIANT))?;
    Ok(OutlineSymbolDto::new(
        candidate.name.clone(),
        protocol_symbol_kind(candidate.kind),
        candidate.symbol_path.clone(),
        u64::try_from(candidate.symbol_path.len().saturating_sub(1)).unwrap_or(u64::MAX),
        candidate.detail.clone(),
        start_line,
        Some(start_column),
        end_line,
        end_column,
        protocol_lsp_range(candidate.lsp_range),
        protocol_lsp_range(candidate.lsp_selection_range),
        SelectionByteSelectorDto::new(candidate.byte_range.start, candidate.byte_range.end),
    ))
}

fn render_human(path: &str, symbols: &[OutlineSymbolDto]) -> Result<String, OutlineErrorDto> {
    let maximum = usize::try_from(MAX_RESPONSE_BYTES).unwrap_or(usize::MAX);
    render_human_with_limit(path, symbols, maximum)
}

fn render_human_with_limit(
    path: &str,
    symbols: &[OutlineSymbolDto],
    maximum: usize,
) -> Result<String, OutlineErrorDto> {
    let mut output = String::new();
    if !symbols.is_empty() {
        push_bounded(
            &mut output,
            format!(
                "{}: {} document symbols\n",
                escape_terminal_text(path),
                symbols.len()
            ),
            maximum,
        )?;
    }
    for symbol in symbols {
        let range = symbol.lsp_range();
        let selector = symbol.selector();
        let indent = "  ".repeat(usize::try_from(symbol.depth()).unwrap_or(0));
        let line = format!(
            "{indent}{kind} {name} lines {start_line}..{end_line} \
             lsp={sl}:{sc}..{el}:{ec} bytes {byte_start}..{byte_end}\n",
            kind = symbol.symbol_kind().as_str(),
            name = escape_terminal_text(symbol.name()),
            start_line = symbol.start_line(),
            end_line = symbol.end_line(),
            sl = range.start().line(),
            sc = range.start().character(),
            el = range.end().line(),
            ec = range.end().character(),
            byte_start = selector.start(),
            byte_end = selector.end(),
        );
        push_bounded(&mut output, line, maximum)?;
    }
    if symbols.is_empty() {
        push_bounded(&mut output, "no document symbols\n".to_owned(), maximum)?;
    }
    Ok(output)
}

fn push_bounded(output: &mut String, chunk: String, maximum: usize) -> Result<(), OutlineErrorDto> {
    let next_size = output
        .len()
        .checked_add(chunk.len())
        .ok_or_else(human_size_error)?;
    if next_size > maximum {
        return Err(human_size_error());
    }
    output.push_str(&chunk);
    Ok(())
}

/// Retargets one shared selection mapper report onto the outline envelope.
///
/// Codes and context carry over verbatim except the two selection-specific
/// request/internal codes, which become their outline equivalents. Messages
/// that name the selection surface are rewritten for the outline surface.
fn retarget_report(report: SelectionErrorDto) -> OutlineErrorDto {
    let code = match report.code() {
        SelectionErrorCode::InvalidSelectionQuery => OutlineErrorCode::InvalidOutlineQuery,
        SelectionErrorCode::LspServerNotConfigured => OutlineErrorCode::LspServerNotConfigured,
        SelectionErrorCode::UnsupportedTextEncoding => OutlineErrorCode::UnsupportedTextEncoding,
        SelectionErrorCode::LspCapabilityUnavailable => OutlineErrorCode::LspCapabilityUnavailable,
        SelectionErrorCode::LspFlatSymbolsUnsupported => {
            OutlineErrorCode::LspFlatSymbolsUnsupported
        }
        SelectionErrorCode::LspDocumentSyncUnavailable => {
            OutlineErrorCode::LspDocumentSyncUnavailable
        }
        SelectionErrorCode::LspResourceLimitExceeded => OutlineErrorCode::LspResourceLimitExceeded,
        SelectionErrorCode::LspTimeout => OutlineErrorCode::LspTimeout,
        SelectionErrorCode::LspStartFailed => OutlineErrorCode::LspStartFailed,
        SelectionErrorCode::LspExited => OutlineErrorCode::LspExited,
        SelectionErrorCode::LspProtocolError => OutlineErrorCode::LspProtocolError,
        SelectionErrorCode::LspRequestFailed => OutlineErrorCode::LspRequestFailed,
        // Unreachable from the query-less outline pipeline; fail closed.
        SelectionErrorCode::SelectionNotFound
        | SelectionErrorCode::SelectionAmbiguous
        | SelectionErrorCode::SelectionInternalError => OutlineErrorCode::OutlineInternalError,
    };
    OutlineErrorDto::new(
        code,
        outline_message(report.message()),
        report.context().clone(),
    )
}

const SELECTION_RESOURCE_MESSAGE: &str = "semantic selection exceeded a configured resource limit";
const SELECTION_TIMEOUT_MESSAGE: &str =
    "the language server did not respond before the selection deadline";
const SELECTION_EXITED_MESSAGE: &str = "the language server exited before selection completed";

fn outline_message(original: &str) -> String {
    match original {
        SELECTION_RESOURCE_MESSAGE => {
            "the read-only outline exceeded a configured resource limit".to_owned()
        }
        SELECTION_TIMEOUT_MESSAGE => {
            "the language server did not respond before the outline deadline".to_owned()
        }
        SELECTION_EXITED_MESSAGE => {
            "the language server exited before the outline completed".to_owned()
        }
        _ => original.to_owned(),
    }
}

fn outline_filesystem_error(error_value: FsError) -> OutlineErrorDto {
    retarget_report(map_filesystem_error(error_value))
}

fn outline_session_error(error_value: SessionError) -> OutlineErrorDto {
    retarget_report(map_session_error(error_value))
}

fn outline_symbol_error(error_value: SymbolError) -> OutlineErrorDto {
    retarget_report(map_symbol_error(error_value))
}

fn outline_protocol_error() -> OutlineErrorDto {
    OutlineErrorDto::new(
        OutlineErrorCode::LspProtocolError,
        "the immutable source position index is invalid",
        BTreeMap::new(),
    )
}

fn outline_internal_error(message: &'static str) -> OutlineErrorDto {
    OutlineErrorDto::new(
        OutlineErrorCode::OutlineInternalError,
        message,
        BTreeMap::new(),
    )
}

fn resource_limit_error(resource: &'static str, limit: usize) -> OutlineErrorDto {
    OutlineErrorDto::new(
        OutlineErrorCode::LspResourceLimitExceeded,
        "the read-only outline exceeded a configured resource limit",
        BTreeMap::from([
            ("limit".to_owned(), json!(limit)),
            ("resource".to_owned(), json!(resource)),
        ]),
    )
}

fn human_size_error() -> OutlineErrorDto {
    resource_limit_error("serialized_human_output", MAX_RESPONSE_BYTES as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gen_lsp_types::{Position, Range};
    use srcmv_core::ByteRange;
    use srcmv_protocol::{
        SelectionKnownSymbolKindDto, SelectionLspPositionDto, SelectionLspRangeDto,
        SelectionSymbolKindDto,
    };

    fn fixture_symbol(
        name: &str,
        kind: SelectionSymbolKindDto,
        depth: u64,
        start_line: u64,
        end_line: u64,
    ) -> OutlineSymbolDto {
        let lsp = u32::try_from(depth).unwrap_or(0);
        OutlineSymbolDto::new(
            name.to_owned(),
            kind,
            vec![name.to_owned()],
            depth,
            None,
            start_line,
            Some(1),
            end_line,
            Some(2),
            outline_lsp_range(2 + lsp, 4 * lsp, 6 + lsp, 5 * lsp),
            outline_lsp_range(2 + lsp, 5 * lsp, 2 + lsp, 10 * lsp),
            SelectionByteSelectorDto::new(19 + 17 * depth, 77 - 2 * depth),
        )
    }

    fn outline_lsp_range(
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) -> SelectionLspRangeDto {
        SelectionLspRangeDto::new(
            SelectionLspPositionDto::new(start_line, start_character),
            SelectionLspPositionDto::new(end_line, end_character),
        )
    }

    fn normalized(kind: NormalizedSymbolKind, name: &str) -> NormalizedSymbol {
        NormalizedSymbol {
            name: name.to_owned(),
            detail: None,
            kind,
            symbol_path: vec![name.to_owned()],
            lsp_range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            lsp_selection_range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            byte_range: ByteRange { start: 0, end: 1 },
            selection_byte_range: ByteRange { start: 0, end: 1 },
        }
    }

    fn selection_report(code: SelectionErrorCode) -> SelectionErrorDto {
        SelectionErrorDto::new(
            code,
            "fixture message",
            BTreeMap::from([("probe".to_owned(), json!(42))]),
        )
    }

    #[test]
    fn unknown_kind_spellings_fail_as_invalid_outline_queries() {
        assert_eq!(
            parse_kinds(&["function".to_owned(), "method".to_owned()])
                .expect("standard spellings must parse"),
            vec![KnownSymbolKind::Function, KnownSymbolKind::Method]
        );

        let error = parse_kinds(&["function".to_owned(), "bogus".to_owned()])
            .expect_err("unknown spellings must fail");
        assert_eq!(error.code(), OutlineErrorCode::InvalidOutlineQuery);
        assert_eq!(error.exit_code(), 2);
        assert!(!error.retryable());
    }

    #[test]
    fn outline_symbol_count_bound_covers_below_at_and_above() {
        assert!(enforce_outline_limit(DEFAULT_MAXIMUM_OUTLINE_SYMBOLS - 1).is_ok());
        assert!(enforce_outline_limit(DEFAULT_MAXIMUM_OUTLINE_SYMBOLS).is_ok());

        let error = enforce_outline_limit(DEFAULT_MAXIMUM_OUTLINE_SYMBOLS + 1)
            .expect_err("above-limit counts must fail");
        assert_eq!(error.code(), OutlineErrorCode::LspResourceLimitExceeded);
        assert_eq!(error.exit_code(), 4);
        assert!(!error.retryable());
        assert_eq!(error.context()["resource"], json!("outline_symbols"));
        assert_eq!(
            error.context()["limit"],
            json!(DEFAULT_MAXIMUM_OUTLINE_SYMBOLS)
        );
    }

    #[test]
    fn shared_mapper_reports_retarget_to_the_outline_envelope() {
        let cases = [
            (
                SelectionErrorCode::InvalidSelectionQuery,
                OutlineErrorCode::InvalidOutlineQuery,
            ),
            (
                SelectionErrorCode::LspServerNotConfigured,
                OutlineErrorCode::LspServerNotConfigured,
            ),
            (
                SelectionErrorCode::SelectionNotFound,
                OutlineErrorCode::OutlineInternalError,
            ),
            (
                SelectionErrorCode::SelectionAmbiguous,
                OutlineErrorCode::OutlineInternalError,
            ),
            (
                SelectionErrorCode::UnsupportedTextEncoding,
                OutlineErrorCode::UnsupportedTextEncoding,
            ),
            (
                SelectionErrorCode::LspCapabilityUnavailable,
                OutlineErrorCode::LspCapabilityUnavailable,
            ),
            (
                SelectionErrorCode::LspFlatSymbolsUnsupported,
                OutlineErrorCode::LspFlatSymbolsUnsupported,
            ),
            (
                SelectionErrorCode::LspDocumentSyncUnavailable,
                OutlineErrorCode::LspDocumentSyncUnavailable,
            ),
            (
                SelectionErrorCode::LspResourceLimitExceeded,
                OutlineErrorCode::LspResourceLimitExceeded,
            ),
            (SelectionErrorCode::LspTimeout, OutlineErrorCode::LspTimeout),
            (
                SelectionErrorCode::LspStartFailed,
                OutlineErrorCode::LspStartFailed,
            ),
            (SelectionErrorCode::LspExited, OutlineErrorCode::LspExited),
            (
                SelectionErrorCode::LspProtocolError,
                OutlineErrorCode::LspProtocolError,
            ),
            (
                SelectionErrorCode::LspRequestFailed,
                OutlineErrorCode::LspRequestFailed,
            ),
            (
                SelectionErrorCode::SelectionInternalError,
                OutlineErrorCode::OutlineInternalError,
            ),
        ];
        // Every registered selection code must have an explicit retarget rule.
        assert_eq!(cases.len(), SelectionErrorCode::ALL.len());

        for (selection_code, expected_code) in cases {
            let retargeted = retarget_report(selection_report(selection_code));
            assert_eq!(
                retargeted.code(),
                expected_code,
                "{} must retarget",
                selection_code.as_str()
            );
            assert_eq!(
                retargeted.retryable(),
                selection_code.retryable(),
                "{} retryability",
                selection_code.as_str()
            );
            assert_eq!(retargeted.context()["probe"], json!(42));
            let serialized = serde_json::to_value(&retargeted).expect("report should serialize");
            assert_eq!(serialized["outline_protocol_version"], 1);
        }
    }

    #[test]
    fn selection_flavored_messages_are_rewritten_for_the_outline_envelope() {
        let rewritten = [
            (
                SELECTION_RESOURCE_MESSAGE,
                "the read-only outline exceeded a configured resource limit",
            ),
            (
                SELECTION_TIMEOUT_MESSAGE,
                "the language server did not respond before the outline deadline",
            ),
            (
                SELECTION_EXITED_MESSAGE,
                "the language server exited before the outline completed",
            ),
        ];
        for (original, expected) in rewritten {
            assert_eq!(outline_message(original), expected);
        }

        assert_eq!(
            outline_message("the source snapshot is not valid UTF-8"),
            "the source snapshot is not valid UTF-8"
        );
    }

    #[test]
    fn kind_filters_keep_everything_when_empty_and_subset_otherwise() {
        let symbols = vec![
            normalized(NormalizedSymbolKind::Known(KnownSymbolKind::Class), "Outer"),
            normalized(
                NormalizedSymbolKind::Known(KnownSymbolKind::Function),
                "alpha",
            ),
            normalized(NormalizedSymbolKind::Unknown(99), "future"),
        ];
        let ordered = order_unique_candidates(&symbols);

        assert_eq!(filter_kinds(&ordered, &[]).len(), 3);

        let functions_only = filter_kinds(&ordered, &[KnownSymbolKind::Function]);
        assert_eq!(functions_only.len(), 1);
        assert_eq!(functions_only[0].name, "alpha");

        let both_requested = filter_kinds(
            &ordered,
            &[KnownSymbolKind::Function, KnownSymbolKind::Class],
        );
        assert_eq!(both_requested.len(), 2);
        assert_eq!(both_requested[0].name, "Outer");
        assert_eq!(both_requested[1].name, "alpha");

        let unknown_unreachable = filter_kinds(&ordered, &[KnownSymbolKind::File]);
        assert!(unknown_unreachable.is_empty());
    }

    #[test]
    fn human_output_indents_roots_at_column_zero_and_nests_by_depth() {
        let outer = OutlineSymbolDto::new(
            "Outer",
            SelectionSymbolKindDto::Known(SelectionKnownSymbolKindDto::Class),
            vec!["Outer".to_owned()],
            0,
            None,
            3,
            Some(1),
            7,
            Some(2),
            outline_lsp_range(2, 0, 6, 1),
            outline_lsp_range(2, 5, 2, 10),
            SelectionByteSelectorDto::new(19, 77),
        );
        let alpha = OutlineSymbolDto::new(
            "alpha",
            SelectionSymbolKindDto::Known(SelectionKnownSymbolKindDto::Function),
            vec!["Outer".to_owned(), "alpha".to_owned()],
            1,
            None,
            4,
            Some(5),
            6,
            Some(6),
            outline_lsp_range(3, 4, 5, 5),
            outline_lsp_range(3, 11, 3, 16),
            SelectionByteSelectorDto::new(36, 75),
        );
        let symbols = [outer, alpha];

        let output = render_human_with_limit("source.rs", &symbols, usize::MAX)
            .expect("small outlines must render");

        assert_eq!(
            output,
            concat!(
                "source.rs: 2 document symbols\n",
                "class Outer lines 3..7 lsp=2:0..6:1 bytes 19..77\n",
                "  function alpha lines 4..6 lsp=3:4..5:5 bytes 36..75\n",
            )
        );
    }

    #[test]
    fn human_output_escapes_control_characters_in_names_and_paths() {
        let symbol = [fixture_symbol(
            "bad\u{1b}name",
            SelectionSymbolKindDto::Unknown,
            0,
            1,
            2,
        )];

        let output = render_human_with_limit("dir\u{7}file.rs", &symbol, usize::MAX)
            .expect("escaped outlines must render");

        assert!(output.starts_with("dir\\u{7}file.rs: 1 document symbols\n"));
        assert!(output.contains("unknown bad\\u{1b}name lines 1..2"));
    }

    #[test]
    fn empty_human_output_prints_no_header_and_the_no_symbols_phrase() {
        let output =
            render_human_with_limit("source.rs", &[], usize::MAX).expect("empty must render");
        assert_eq!(output, "no document symbols\n");
    }

    #[test]
    fn human_accumulation_fails_closed_at_and_below_the_cap_without_partial_output() {
        let symbols = [
            fixture_symbol(
                "Outer",
                SelectionSymbolKindDto::Known(SelectionKnownSymbolKindDto::Class),
                0,
                3,
                7,
            ),
            fixture_symbol(
                "alpha",
                SelectionSymbolKindDto::Known(SelectionKnownSymbolKindDto::Function),
                1,
                4,
                6,
            ),
        ];

        let exact = render_human_with_limit("source.rs", &symbols, usize::MAX)
            .expect("reference render must succeed")
            .len();
        assert!(
            render_human_with_limit("source.rs", &symbols, exact).is_ok(),
            "at-limit renders must succeed"
        );

        let error = render_human_with_limit("source.rs", &symbols, exact - 1)
            .expect_err("above-limit renders must fail");
        assert_eq!(error.code(), OutlineErrorCode::LspResourceLimitExceeded);
        assert_eq!(
            error.context()["resource"],
            json!("serialized_human_output")
        );
    }
}
