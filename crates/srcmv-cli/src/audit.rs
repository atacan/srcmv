//! Independent conservation-audit command orchestration
//! (`srcmv audit-conservation`).
//!
//! Realizes plan §6 (user-facing command), §11.2 (bounded human report), and
//! §12 (failure mapping) for the read-only, independently versioned
//! `conservation-audit-v1` surface. Like [`crate::select`] this module owns
//! its grammar struct, orchestration function, and rendering; unlike
//! selection it renders every failure through the shared frozen
//! [`ErrorDto`](srcmv_protocol::ErrorDto) registry, so `run_with_io`'s
//! standard `CommandFailure::Edit` path and `render_error` discipline apply
//! unchanged (including its raw-arguments `--json` pre-scan governing error
//! output mode).
//!
//! # Invocation pipeline
//!
//! parse arguments → validate option constraints → reject `--workspace`
//! through the shared target-independent helper → resolve each `TREE`
//! (`@MANIFEST` detection; manifest paths resolve against the manifest's own
//! parent directory per §6.3) → ingest both trees under release
//! [`AuditTreeLimits`](srcmv_fs::AuditTreeLimits) defaults → compute each
//! [`InputTreeSummary`](srcmv_protocol::InputTreeSummary) from ingestion →
//! project ingested files onto borrowed
//! [`AuditFile`](srcmv_core::AuditFile) slices → run the pure engine with
//! release budget defaults (no budget flags exist) → build the bounded
//! protocol report → render exactly one JSON line or the bounded human
//! summary. Any skip is passed through as [`SkipInfo`](srcmv_protocol::SkipInfo);
//! the protocol layer turns skips into the fatal `unreadable_input_skipped`
//! reason and an `inconclusive` verdict. Line indexes stay lazy upstream.
//!
//! Completed audits exit 0 on all three verdicts: the verdict is data, not a
//! process failure (§6.2). `--json` emits the audit report itself — exactly
//! one bounded UTF-8 JSON value plus LF on stdout, matching the frozen
//! schema — never wrapped in any protocol success envelope. Diagnostics go
//! to stderr through the standard envelope.
//!
//! # Error mapping (plan §12)
//!
//! | Source | Registry code | Exit |
//! |---|---|---|
//! | clap grammar violation | `INVALID_CLI` | 2 |
//! | option-constraint violation (`--min-fragment-bytes < 16`,
//!   `--min-substantial-bytes < --min-fragment-bytes`) or empty `@MANIFEST`
//!   spelling | `INVALID_REQUEST` | 2 |
//! | root exists but is not a directory (`FsError::WorkspaceRootNotDirectory`)
//!   — violates §6.1's "both must resolve to existing readable directories" |
//!   `INVALID_REQUEST` | 2 |
//! | manifest syntax problems and missing manifest entries
//!   (`FsError::InvalidPath`, `FsError::PathNotFound` from manifest
//!   ingestion — knowable before any analysis, so fail fast) |
//!   `INVALID_REQUEST` | 2 |
//! | walk-time non-UTF-8 entry names (`FsError::InvalidPath` raised by
//!   directory ingestion) | `IO_ERROR` | 8 |
//! | missing/unreadable root or manifest and canonicalization failures
//!   (`FsError::Io`) | `IO_ERROR` | 8 |
//! | symbolic links anywhere beneath or into a tree (`FsError::SymlinkNotAllowed`) | `SYMLINK_NOT_ALLOWED` | 4 |
//! | non-regular files (`FsError::UnsupportedFileType`) | `UNSUPPORTED_FILE_TYPE` | 4 |
//! | tree limits (`FsError::ResourceLimitExceeded`) and engine budget
//!   exhaustion before any analysis (`CoreError::ResourceLimitExceeded`) |
//!   `RESOURCE_LIMIT_EXCEEDED` | 4 |
//! | unserializable report skeleton (`ProtocolError` limit envelope from
//!   `build_audit_report`) | `RESOURCE_LIMIT_EXCEEDED` | 4 |
//! | engine invariant violations (`CoreError::InvalidDomainValue`,
//!   `FsError::InternalInvariant`, malformed prepared reports) |
//!   `INTERNAL_ERROR` | 8 |
//!
//! The two `FsError::InvalidPath` rows are distinguished deliberately by
//! ingestion form: manifest lines are UTF-8 text whose spellings validate per
//! line, so any rejection there is caller input error, while directory walks
//! meet arbitrary filesystem bytes, so an unrepresentable entry name is an
//! I/O failure of the walk itself.

use std::collections::BTreeMap;
use std::path::Path;

use clap::Args;
use serde_json::{Value, json};
use srcmv_core::{
    AuditFile, ConservationBudget, ConservationInput, ConservationOptions, CoreError,
};
use srcmv_fs::{
    AuditTreeFile, AuditTreeLimits, FsError, SkippedInput, read_audit_tree,
    read_audit_tree_from_manifest,
};
use srcmv_protocol::{
    AuditInputKind, AuditReport, ErrorCode, ErrorDto, InputTreeSummary, ProtocolError, SkipInfo,
    audit_report_json_line, build_audit_report, escape_terminal_text,
};

/// Normative index-window width `K = 16` (§8.2): the fragment floor cannot be
/// set below it so every matchable span contains a full probe window.
const INDEX_WINDOW_BYTES: u64 = 16;
/// Release-default fragment floor (§6.1): equal to the index window.
const DEFAULT_MIN_FRAGMENT_BYTES: u64 = INDEX_WINDOW_BYTES;
/// Release-default substantiality floor (§9.4).
const DEFAULT_MIN_SUBSTANTIAL_BYTES: u64 = 32;
/// Human-summary cap mirroring preview's rendered-diff bound (§11.2).
const AUDIT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Explicit marker appended once when listing overflow truncates the summary.
const TRUNCATION_MARKER: &str = "(truncated)\n";

/// Arguments accepted by `srcmv audit-conservation`.
#[derive(Debug, Args)]
pub(crate) struct AuditArgs {
    /// Before tree: an existing readable directory or `@MANIFEST` path.
    #[arg(long, value_name = "TREE", required = true)]
    base: String,
    /// After tree: an existing readable directory or `@MANIFEST` path.
    #[arg(long, value_name = "TREE", required = true)]
    head: String,
    /// Emit the machine report instead of the human summary.
    #[arg(long)]
    json: bool,
    /// Smallest unmatched residue worth failing over, in bytes (default 32).
    #[arg(long, value_name = "N", default_value_t = DEFAULT_MIN_SUBSTANTIAL_BYTES)]
    min_substantial_bytes: u64,
    /// Smallest acceptable matched fragment, in bytes (default 16).
    #[arg(long, value_name = "N", default_value_t = DEFAULT_MIN_FRAGMENT_BYTES)]
    min_fragment_bytes: u64,
    /// Downgrade multiplicity-counted matches to inconclusive contributors.
    #[arg(long)]
    strict_uniqueness: bool,
    /// Treat formatting-only deltas as failures too.
    #[arg(long)]
    count_formatting: bool,
}

impl AuditArgs {
    /// Returns whether the caller requested JSON output; used by the dispatch
    /// arm before transferring ownership for the workspace rejection helper.
    pub(crate) const fn json(&self) -> bool {
        self.json
    }
}

/// One resolved input tree: ingested files, skip records, and wire kind.
#[derive(Debug)]
struct IngestedTree {
    kind: AuditInputKind,
    files: Vec<AuditTreeFile>,
    skips: Vec<SkippedInput>,
}

impl IngestedTree {
    fn summary(&self) -> Result<InputTreeSummary, ErrorDto> {
        let bytes = self.files.iter().try_fold(0_u64, |total, file| {
            total.checked_add(u64::try_from(file.bytes.len()).unwrap_or(u64::MAX))
        });
        Ok(InputTreeSummary::new(
            self.kind,
            u64::try_from(self.files.len()).unwrap_or(u64::MAX),
            bytes.ok_or_else(|| internal_error("audit_tree_byte_total_overflow"))?,
        ))
    }
}

/// Which supplied form produced one tree; drives `FsError::InvalidPath`
/// disambiguation in the documented error table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeForm {
    /// Recursively read directory.
    Directory,
    /// `@MANIFEST`-listed subset.
    Manifest,
}

/// Executes one complete read-only conservation audit.
///
/// Failures return the shared registry report plus the caller's requested
/// output mode so `run_with_io` renders through the standard envelope.
pub(crate) fn execute(arguments: AuditArgs) -> Result<String, (ErrorDto, bool)> {
    let json_output = arguments.json();
    execute_inner(arguments).map_err(|report| (report, json_output))
}

fn execute_inner(arguments: AuditArgs) -> Result<String, ErrorDto> {
    let options = validate_options(
        arguments.min_substantial_bytes,
        arguments.min_fragment_bytes,
        arguments.strict_uniqueness,
        arguments.count_formatting,
    )?;
    let base_spelling = arguments.base.clone();
    let head_spelling = arguments.head.clone();

    let base = ingest_tree(&base_spelling)?;
    let head = ingest_tree(&head_spelling)?;

    let base_files = project(&base.files);
    let head_files = project(&head.files);
    let skip_infos = base
        .skips
        .iter()
        .chain(head.skips.iter())
        .map(|skip| SkipInfo::new(skip.path.value.clone()))
        .collect::<Vec<_>>();

    let outcome = srcmv_core::audit_conservation(ConservationInput {
        base: &base_files,
        head: &head_files,
        options,
        budget: ConservationBudget::default(),
    })
    .map_err(map_engine_error)?;

    let report = build_audit_report(
        &outcome,
        &options,
        (base.summary()?, head.summary()?),
        &skip_infos,
        &head_files,
    )
    .map_err(ProtocolError::into_report)?;

    if arguments.json {
        audit_report_json_line(&report).map_err(ProtocolError::into_report)
    } else {
        render_human(&report, &base_spelling, &head_spelling)
    }
}

/// Validates the §6.1 resolution constraints after clap parsing.
///
/// Every accepted value satisfies `min_fragment_bytes >= K = 16` and
/// `min_substantial_bytes >= min_fragment_bytes`; violations are
/// `INVALID_REQUEST` (exit 2).
fn validate_options(
    min_substantial_bytes: u64,
    min_fragment_bytes: u64,
    strict_uniqueness: bool,
    count_formatting: bool,
) -> Result<ConservationOptions, ErrorDto> {
    if min_fragment_bytes < INDEX_WINDOW_BYTES {
        return Err(invalid_request(
            "--min-fragment-bytes must be at least the 16-byte index window",
            "min_fragment_below_index_window",
        ));
    }
    if min_substantial_bytes < min_fragment_bytes {
        return Err(invalid_request(
            "--min-substantial-bytes must be at least --min-fragment-bytes",
            "min_substantial_below_min_fragment",
        ));
    }
    ConservationOptions::new(
        min_substantial_bytes,
        min_fragment_bytes,
        strict_uniqueness,
        count_formatting,
    )
    .map_err(|_| internal_error("validated_options_rejected_by_engine"))
}

/// Resolves one supplied `TREE` spelling and ingests it under release limits.
fn ingest_tree(spelling: &str) -> Result<IngestedTree, ErrorDto> {
    if let Some(manifest) = spelling.strip_prefix('@') {
        if manifest.is_empty() {
            return Err(invalid_request(
                "the @MANIFEST spelling requires a manifest path",
                "empty_manifest_path",
            ));
        }
        let (files, skips) = read_audit_tree_from_manifest(Path::new(manifest), limits())
            .map_err(|error| map_filesystem_error(error, TreeForm::Manifest))?;
        Ok(IngestedTree {
            kind: AuditInputKind::Manifest,
            files,
            skips,
        })
    } else {
        let (files, skips) = read_audit_tree(Path::new(spelling), limits())
            .map_err(|error| map_filesystem_error(error, TreeForm::Directory))?;
        Ok(IngestedTree {
            kind: AuditInputKind::Directory,
            files,
            skips,
        })
    }
}

/// Release audit-tree limits; no limit flags exist on the grammar.
fn limits() -> AuditTreeLimits {
    AuditTreeLimits::default()
}

/// Projects ingested files onto the engine's borrowed representation.
fn project(files: &[AuditTreeFile]) -> Vec<AuditFile<'_>> {
    files
        .iter()
        .map(|file| AuditFile {
            path: file.path.value.as_bytes(),
            bytes: file.bytes.as_ref(),
            digest: &file.digest,
        })
        .collect()
}

/// Maps filesystem errors onto the shared registry per the module table.
fn map_filesystem_error(error: FsError, form: TreeForm) -> ErrorDto {
    match error {
        FsError::UnsupportedPlatform => ErrorDto::new(
            ErrorCode::UnsupportedPlatform,
            "the conservation audit requires Linux or macOS",
            BTreeMap::new(),
        ),
        FsError::WorkspaceRootNotDirectory => ErrorDto::new(
            ErrorCode::InvalidRequest,
            "a supplied tree root is not an existing readable directory",
            BTreeMap::from([("reason".to_string(), json!("root_not_directory"))]),
        ),
        FsError::SymlinkNotAllowed { path } => ErrorDto::new(
            ErrorCode::SymlinkNotAllowed,
            "an audit tree contains or traverses a symbolic link",
            BTreeMap::from([("path".to_string(), json!(path))]),
        ),
        FsError::UnsupportedFileType { path } => ErrorDto::new(
            ErrorCode::UnsupportedFileType,
            "an audit tree entry is not a regular file",
            BTreeMap::from([("path".to_string(), json!(path))]),
        ),
        FsError::ResourceLimitExceeded {
            resource,
            actual,
            limit,
        } => ErrorDto::new(
            ErrorCode::ResourceLimitExceeded,
            "an audit tree exceeds a resource limit",
            BTreeMap::from([
                ("actual".to_string(), json!(actual)),
                ("limit".to_string(), json!(limit)),
                ("resource".to_string(), json!(resource)),
            ]),
        ),
        FsError::InvalidPath { path, reason } => match form {
            // Manifest spellings are UTF-8 request input; rejection here is a
            // caller error even when the reason names entry encoding.
            TreeForm::Manifest => ErrorDto::new(
                ErrorCode::InvalidRequest,
                "a manifest line is not a valid relative path",
                BTreeMap::from([
                    (
                        "path".to_string(),
                        json!(srcmv_protocol::redact_path(&path)),
                    ),
                    ("reason".to_string(), json!(reason)),
                ]),
            ),
            // Walk-time entry names come from arbitrary filesystem bytes; a
            // non-representable name is an I/O failure of the walk (exit 8).
            TreeForm::Directory => io_error_dto("list_audit_directory", Some(path)),
        },
        // Only strict manifest ingestion produces PathNotFound: a listed but
        // absent entry is knowable before analysis, so it fails fast (exit 2).
        FsError::PathNotFound { path } => ErrorDto::new(
            ErrorCode::InvalidRequest,
            "a manifest entry does not exist",
            BTreeMap::from([("path".to_string(), json!(path))]),
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
            ErrorDto::new(ErrorCode::IoError, "audit tree ingestion failed", context)
        }
        FsError::InternalInvariant { invariant } => internal_error_named(invariant),
        other => ErrorDto::new(
            ErrorCode::InternalError,
            "an unrecognized filesystem error occurred during tree ingestion",
            BTreeMap::from([("reason".to_string(), json!(other.to_string()))]),
        ),
    }
}

/// Maps pure-engine errors onto the registry: budget exhaustion before any
/// analysis completes is `RESOURCE_LIMIT_EXCEEDED`; every invariant violation
/// fails loudly as `INTERNAL_ERROR`.
fn map_engine_error(error: CoreError) -> ErrorDto {
    match error {
        CoreError::ResourceLimitExceeded {
            resource,
            actual,
            limit,
        } => ErrorDto::new(
            ErrorCode::ResourceLimitExceeded,
            "the conservation audit exceeded an engine resource budget",
            BTreeMap::from([
                ("actual".to_string(), json!(actual)),
                ("limit".to_string(), json!(limit)),
                ("resource".to_string(), json!(resource)),
            ]),
        ),
        other => ErrorDto::new(
            ErrorCode::InternalError,
            "the conservation engine rejected its inputs",
            BTreeMap::from([("reason".to_string(), json!(other.to_string()))]),
        ),
    }
}

fn io_error_dto(operation: &str, path: Option<String>) -> ErrorDto {
    let mut context = BTreeMap::from([("operation".to_string(), json!(operation))]);
    if let Some(path) = path {
        context.insert("path".to_string(), json!(path));
    }
    ErrorDto::new(ErrorCode::IoError, "audit tree ingestion failed", context)
}

fn invalid_request(message: &str, reason: &'static str) -> ErrorDto {
    ErrorDto::new(
        ErrorCode::InvalidRequest,
        message,
        BTreeMap::from([("reason".to_string(), json!(reason))]),
    )
}

fn internal_error(reason: &'static str) -> ErrorDto {
    ErrorDto::new(
        ErrorCode::InternalError,
        "an internal conservation-audit command failure occurred",
        BTreeMap::from([("invariant".to_string(), json!(reason))]),
    )
}

fn internal_error_named(invariant: &str) -> ErrorDto {
    ErrorDto::new(
        ErrorCode::InternalError,
        "an internal conservation-audit command failure occurred",
        BTreeMap::from([("invariant".to_string(), json!(invariant))]),
    )
}

/// Renders the bounded human summary (§11.2) from the built report so the
/// text always mirrors the JSON contract's listings, ordering, and verdict.
fn render_human(
    report: &AuditReport,
    base_spelling: &str,
    head_spelling: &str,
) -> Result<String, ErrorDto> {
    let value = serde_json::to_value(report)
        .map_err(|_| internal_error("human_report_serialization_failed"))?;
    compose_human(&value, base_spelling, head_spelling, AUDIT_OUTPUT_BYTES)
}

/// Fixed summary lines that are always rendered, reserved before listings.
fn human_skeleton(
    value: &Value,
    base_spelling: &str,
    head_spelling: &str,
) -> Result<(String, String), ErrorDto> {
    let inputs = object(value, "inputs")?;
    let mut head = "conservation audit v1\n".to_owned();
    head.push_str(&input_line("base", base_spelling, object(inputs, "base")?));
    head.push_str(&input_line("head", head_spelling, object(inputs, "head")?));
    let verdict = text(value, "verdict")?.to_ascii_uppercase();
    head.push_str(&format!("verdict: {verdict}\n"));
    head.push_str(&totals_line(object(value, "totals")?));

    let reasons = string_list(value, "inconclusive_reasons")?;
    let warnings = string_list(value, "warnings")?;
    let reasons_rendered = if reasons.is_empty() {
        "(none)".to_owned()
    } else {
        reasons.join(", ")
    };
    let warnings_rendered = if warnings.is_empty() {
        "(none)".to_owned()
    } else {
        warnings.join(", ")
    };
    let tail = format!("inconclusive reasons: {reasons_rendered}\nwarnings: {warnings_rendered}\n");
    Ok((head, tail))
}

fn input_line(label: &str, spelling: &str, summary: &Value) -> String {
    let safe = escape_terminal_text(spelling);
    let files = u64_of(summary, "files");
    let bytes = u64_of(summary, "bytes");
    let shape = if text(summary, "kind").is_ok_and(|kind| kind == "manifest") {
        format!("(manifest; {files} files, {bytes} bytes)")
    } else {
        format!("({files} files, {bytes} bytes)")
    };
    format!("{label}:  {safe}  {shape}\n")
}

fn totals_line(totals: &Value) -> String {
    format!(
        "removed {} B | conserved {} B | unmatched {} B | formatting {} B | new {} B\n",
        u64_of(totals, "removed_bytes"),
        u64_of(totals, "conserved_bytes"),
        u64_of(totals, "unmatched_substantial_bytes"),
        u64_of(totals, "formatting_only_bytes"),
        u64_of(totals, "new_bytes"),
    )
}

/// Assembles the full summary under `maximum` bytes: skeleton reserved first,
/// listings filling the remainder, one explicit `(truncated)` marker on
/// overflow, and inconclusive reasons always rendered (§11.2–§11.3).
fn compose_human(
    value: &Value,
    base_spelling: &str,
    head_spelling: &str,
    maximum: usize,
) -> Result<String, ErrorDto> {
    let (head, tail) = human_skeleton(value, base_spelling, head_spelling)?;
    let mut writer = HumanWriter::new(head.len(), tail.len(), maximum)?;

    let unmatched = array(value, "unmatched")?;
    if !unmatched.is_empty() {
        writer.push(&format!("unmatched regions ({}):\n", unmatched.len()));
        for entry in unmatched {
            writer.push(&region_item(entry)?);
        }
    }
    let formatting = array(value, "formatting")?;
    if !formatting.is_empty() {
        writer.push(&format!(
            "formatting-only regions ({}):\n",
            formatting.len()
        ));
        for entry in formatting {
            writer.push(&region_item(entry)?);
        }
    }
    let new_content = array(value, "new_content")?;
    if !new_content.is_empty() {
        writer.push("top new content:\n");
        for span in new_content {
            writer.push(&new_span_item(span)?);
        }
    }

    Ok(writer.finish(head, tail))
}

/// Byte-budgeted listing writer mirroring preview's `DiffWriter` discipline:
/// the fixed skeleton is reserved up front, listings fill the remainder, and
/// the first overflow appends the explicit marker and refuses further lines.
#[derive(Debug)]
struct HumanWriter {
    body: String,
    allowance: usize,
    truncated: bool,
}

impl HumanWriter {
    fn new(reserved_head: usize, reserved_tail: usize, maximum: usize) -> Result<Self, ErrorDto> {
        let committed = reserved_head
            .checked_add(reserved_tail)
            .and_then(|sum| sum.checked_add(TRUNCATION_MARKER.len()))
            .ok_or_else(|| summary_limit_error(u64::MAX))?;
        if committed > maximum {
            return Err(summary_limit_error(
                u64::try_from(committed).unwrap_or(u64::MAX),
            ));
        }
        Ok(Self {
            body: String::new(),
            allowance: maximum - committed,
            truncated: false,
        })
    }

    fn push(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        if self.body.len().saturating_add(line.len()) > self.allowance {
            self.truncated = true;
            self.body.push_str(TRUNCATION_MARKER);
            return;
        }
        self.body.push_str(line);
    }

    fn finish(self, head: String, tail: String) -> String {
        let mut out = head;
        out.push_str(&self.body);
        out.push_str(&tail);
        out
    }
}

fn summary_limit_error(actual: u64) -> ErrorDto {
    ErrorDto::new(
        ErrorCode::ResourceLimitExceeded,
        "the human conservation-audit summary exceeds its output limit",
        BTreeMap::from([
            ("actual".to_string(), json!(actual)),
            ("limit".to_string(), json!(AUDIT_OUTPUT_BYTES)),
            ("resource".to_string(), json!("human_audit_summary")),
        ]),
    )
}

/// One unmatched/formatting listing row: `(lines a..b)` is omitted entirely
/// for null ranges (binary content, §9.5); digests always render.
fn region_item(entry: &Value) -> Result<String, ErrorDto> {
    let path = escape_terminal_text(text(entry, "source_path")?);
    let (start, end) = byte_range(entry, "source_byte_range")?;
    let length = number(entry, "length")?;
    let digest = text(entry, "sha256")?;
    let lines = match entry.get("source_line_range") {
        Some(Value::Array(pair)) if pair.len() == 2 => {
            format!(" (lines {}..{})", index_u64(pair, 0)?, index_u64(pair, 1)?)
        }
        _ => String::new(),
    };
    Ok(format!(
        "  - {path} [{start}..{end}){lines} {length} B {digest}\n"
    ))
}

/// Top-new-content rows follow the §11.2 sample: byte range and length only.
fn new_span_item(span: &Value) -> Result<String, ErrorDto> {
    let path = escape_terminal_text(text(span, "path")?);
    let (start, end) = byte_range(span, "byte_range")?;
    let length = number(span, "length")?;
    Ok(format!("  - {path} [{start}..{end}) {length} B\n"))
}

fn object<'a>(value: &'a Value, key: &str) -> Result<&'a Value, ErrorDto> {
    value
        .get(key)
        .ok_or_else(|| internal_error("human_report_object_missing"))
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, ErrorDto> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| internal_error("human_report_array_missing"))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, ErrorDto> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| internal_error("human_report_text_missing"))
}

fn number(value: &Value, key: &str) -> Result<u64, ErrorDto> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| internal_error("human_report_number_missing"))
}

fn u64_of(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn byte_range(value: &Value, key: &str) -> Result<(u64, u64), ErrorDto> {
    let pair = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| internal_error("human_report_range_missing"))?;
    Ok((index_u64(pair, 0)?, index_u64(pair, 1)?))
}

fn index_u64(pair: &[Value], index: usize) -> Result<u64, ErrorDto> {
    pair.get(index)
        .and_then(Value::as_u64)
        .ok_or_else(|| internal_error("human_report_range_element_missing"))
}

fn string_list(value: &Value, key: &str) -> Result<Vec<String>, ErrorDto> {
    array(value, key)?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| internal_error("human_report_string_missing"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(min_substantial: u64, min_fragment: u64) -> Result<ConservationOptions, ErrorDto> {
        validate_options(min_substantial, min_fragment, false, false)
    }

    #[test]
    fn option_floors_should_enforce_window_and_pairwise_constraints_below_at_above() {
        let exit = |result: Result<ConservationOptions, ErrorDto>| {
            result.err().map(|error| error.exit_code())
        };
        assert_eq!(
            exit(options(32, 15)),
            Some(2),
            "below the window K must fail INVALID_REQUEST"
        );
        assert!(options(32, 16).is_ok(), "at the window K must pass");
        assert!(options(33, 17).is_ok(), "above both floors must pass");
        assert!(
            options(16, 16).is_ok(),
            "equal floors are the maximal practical strictness"
        );
        assert_eq!(
            exit(options(15, 16)),
            Some(2),
            "substantial below fragment must fail INVALID_REQUEST"
        );
        assert_eq!(
            options(32, 16)
                .expect("defaults should validate")
                .min_fragment_bytes(),
            INDEX_WINDOW_BYTES
        );
    }

    #[test]
    fn filesystem_errors_should_map_onto_the_shared_registry_per_the_documented_table() {
        use std::io;

        let cases: Vec<(FsError, TreeForm, ErrorCode, u8)> = vec![
            (
                FsError::WorkspaceRootNotDirectory,
                TreeForm::Directory,
                ErrorCode::InvalidRequest,
                2,
            ),
            (
                FsError::Io {
                    operation: "canonicalize_workspace",
                    path: None,
                    kind: io::ErrorKind::NotFound,
                },
                TreeForm::Directory,
                ErrorCode::IoError,
                8,
            ),
            (
                FsError::Io {
                    operation: "canonicalize_manifest",
                    path: None,
                    kind: io::ErrorKind::NotFound,
                },
                TreeForm::Manifest,
                ErrorCode::IoError,
                8,
            ),
            (
                FsError::InvalidPath {
                    path: "../escape.txt".into(),
                    reason: "parent_escape",
                },
                TreeForm::Manifest,
                ErrorCode::InvalidRequest,
                2,
            ),
            (
                FsError::InvalidPath {
                    path: "bad".into(),
                    reason: "entry_not_utf8",
                },
                TreeForm::Directory,
                ErrorCode::IoError,
                8,
            ),
            (
                FsError::PathNotFound {
                    path: "ghost.txt".into(),
                },
                TreeForm::Manifest,
                ErrorCode::InvalidRequest,
                2,
            ),
            (
                FsError::SymlinkNotAllowed {
                    path: "link.txt".into(),
                },
                TreeForm::Directory,
                ErrorCode::SymlinkNotAllowed,
                4,
            ),
            (
                FsError::UnsupportedFileType {
                    path: "fifo".into(),
                },
                TreeForm::Directory,
                ErrorCode::UnsupportedFileType,
                4,
            ),
            (
                FsError::ResourceLimitExceeded {
                    resource: "audit_tree_file_bytes",
                    actual: 256 * 1024 * 1024 + 1,
                    limit: 256 * 1024 * 1024,
                },
                TreeForm::Directory,
                ErrorCode::ResourceLimitExceeded,
                4,
            ),
            (
                FsError::InternalInvariant {
                    invariant: "test_invariant",
                },
                TreeForm::Directory,
                ErrorCode::InternalError,
                8,
            ),
        ];

        for (error, form, expected_code, expected_exit) in cases {
            let report = map_filesystem_error(error, form);
            assert_eq!(report.code(), expected_code, "{expected_code:?}");
            assert_eq!(report.exit_code(), expected_exit);
        }
    }

    #[test]
    fn engine_errors_should_separate_budget_exhaustion_from_invariants() {
        let budget = map_engine_error(CoreError::ResourceLimitExceeded {
            resource: "conservation_matching_work_units",
            actual: 5,
            limit: 4,
        });
        assert_eq!(budget.code(), ErrorCode::ResourceLimitExceeded);
        assert_eq!(budget.exit_code(), 4);

        let invariant = map_engine_error(CoreError::InvalidDomainValue {
            field: "conservation_ledger_identity",
        });
        assert_eq!(invariant.code(), ErrorCode::InternalError);
        assert_eq!(invariant.exit_code(), 8);
    }

    #[test]
    fn empty_manifest_spelling_should_fail_as_invalid_request_exit_two() {
        let error = ingest_tree("@").expect_err("bare @ must be rejected");
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
        assert_eq!(error.exit_code(), 2);
    }
    fn sample_value() -> Value {
        json!({
            "verdict": "fail",
            "inconclusive_reasons": [],
            "inputs": {
                "base": {"kind": "directory", "files": 142, "bytes": 3204551},
                "head": {"kind": "manifest", "files": 143, "bytes": 3211002}
            },
            "totals": {
                "removed_bytes": 12344,
                "conserved_bytes": 12002,
                "unmatched_substantial_bytes": 110,
                "formatting_only_bytes": 210,
                "new_bytes": 1905
            },
            "unmatched": [
                {
                    "source_path": "src/legacy/parser.ts",
                    "source_byte_range": [220, 330],
                    "source_line_range": [9, 11],
                    "length": 110,
                    "sha256": "sha256:ab41"
                }
            ],
            "formatting": [
                {
                    "source_path": "src/legacy/parser.ts",
                    "source_byte_range": [512, 722],
                    "source_line_range": null,
                    "length": 210,
                    "sha256": "sha256:c3d4"
                }
            ],
            "new_content": [
                {"path": "src/parse/index.ts", "byte_range": [0, 431], "length": 431}
            ],
            "warnings": []
        })
    }

    #[test]
    fn human_summary_should_follow_the_sample_shape_with_binary_ranges_and_escapes() {
        let mut value = sample_value();
        value["unmatched"][0]["source_path"] = json!("evil\u{202e}\u{7}.txt");
        let output = compose_human(&value, "/tmp/base", "@list.txt", AUDIT_OUTPUT_BYTES)
            .expect("sample should render");

        let expected = "\
conservation audit v1
base:  /tmp/base  (142 files, 3204551 bytes)
head:  @list.txt  (manifest; 143 files, 3211002 bytes)
verdict: FAIL
removed 12344 B | conserved 12002 B | unmatched 110 B | formatting 210 B | new 1905 B
unmatched regions (1):
  - evil\\u{202e}\\u{7}.txt [220..330) (lines 9..11) 110 B sha256:ab41
formatting-only regions (1):
  - src/legacy/parser.ts [512..722) 210 B sha256:c3d4
top new content:
  - src/parse/index.ts [0..431) 431 B
inconclusive reasons: (none)
warnings: (none)
";
        assert_eq!(output, expected);
    }

    #[test]
    fn human_summary_should_always_render_inconclusive_reasons_and_truncation_marker() {
        let mut value = sample_value();
        value["inconclusive_reasons"] = json!(["work_budget_exceeded", "ambiguous_common"]);
        let output = compose_human(&value, "b", "h", 480).expect("small cap should render");

        assert!(output.contains("(truncated)\n"), "{output}");
        assert!(
            output.ends_with(
                "inconclusive reasons: work_budget_exceeded, ambiguous_common\nwarnings: (none)\n"
            ),
            "{output}"
        );
    }

    #[test]
    fn human_writer_should_refuse_a_skeleton_that_cannot_fit() {
        let error = HumanWriter::new(AUDIT_OUTPUT_BYTES + 1, 0, AUDIT_OUTPUT_BYTES)
            .expect_err("oversized skeleton must fail");
        assert_eq!(error.code(), ErrorCode::ResourceLimitExceeded);
        assert_eq!(error.exit_code(), 4);
    }
}
