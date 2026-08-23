//! `conservation-audit-v1` report DTOs, bounded rendering, and wire emission.
//!
//! This module realizes plan §11 (report formats) and §11.3 (bounded
//! rendering) for the independently versioned conservation-audit surface: it
//! converts a pure [`ConservationOutcome`] from `srcmv-core` into the frozen
//! JSON contract documented by
//! `docs/schema/conservation-audit-v1/report.schema.json`, applies the §11.3
//! policy table under lowerable [`AuditRenderLimits`], and emits exactly one
//! UTF-8 JSON value plus LF through the shared [`to_json_line`] boundary.
//!
//! Failure handling reuses the shared frozen protocol-v1 registry (§12–§13):
//! skeleton overflow maps to [`ErrorCode::ResourceLimitExceeded`], and every
//! internal invariant violation fails loudly with
//! [`ErrorCode::InternalError`] instead of fabricating a verdict.
//!
//! Truncation discipline (§9.3, §11.3): dropping any `unmatched[]` or
//! `formatting[]` listing entry appends the fatal `report_truncated` reason
//! and forces [`Verdict::Inconclusive`] downstream, so truncation can never
//! manufacture a `pass`. Dropping conserved `regions[]` entries is silent
//! apart from the optional `omitted_region_count` sibling and never changes
//! the verdict. Totals, verdict, reasons, options, and inputs are reserved
//! and never dropped.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};
use srcmv_core::{
    AuditFile, ConservationOptions, ConservationOutcome, InconclusiveReason as EngineReason,
    LineIndex, RegionRecord, RegionStatus as EngineStatus, Totals as EngineTotals,
    Verdict as EngineVerdict,
};

use crate::{ErrorCode, ErrorDto, ProtocolError, to_json_line};

/// The only conservation-audit surface version supported by this release.
pub const CONSERVATION_AUDIT_VERSION: u64 = 1;

/// Release cap on listed conserved regions (§10).
pub const MAX_LISTED_REGIONS: usize = 2_000;

/// Release cap on listed unmatched-substantial entries (§10).
pub const MAX_UNMATCHED_LISTINGS: usize = 200;

/// Release cap on listed formatting-only entries (§10).
pub const MAX_FORMATTING_LISTINGS: usize = 200;

/// Release cap on listed new-content spans (§10).
pub const MAX_NEW_CONTENT_SPANS: usize = 50;

/// Release cap on the serialized report in bytes, trailing LF included (§10).
pub const MAX_SERIALIZED_REPORT_BYTES: u64 = 16 * 1024 * 1024;

/// House per-call line cap reused when numbering new-content spans (§5, §7).
const SPAN_LINE_INDEX_MAX_LINES: u64 = 5_000_000;

/// House per-call line-index memory cap reused when numbering spans (§5, §7).
const SPAN_LINE_INDEX_MEMORY_BYTES: u64 = 256 * 1024 * 1024;

/// Closed verdict vocabulary spelled exactly as the frozen schema enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditVerdict {
    /// Every substantial removed byte was verified conserved.
    Pass,
    /// At least one substantial removed region found no surviving occurrence.
    Fail,
    /// No conservation claim is made; consult the reason list.
    Inconclusive,
}

impl AuditVerdict {
    /// Returns the frozen lowercase wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Inconclusive => "inconclusive",
        }
    }

    const fn from_engine(verdict: EngineVerdict) -> Self {
        match verdict {
            EngineVerdict::Pass => Self::Pass,
            EngineVerdict::Fail => Self::Fail,
            EngineVerdict::Inconclusive => Self::Inconclusive,
        }
    }
}

impl Serialize for AuditVerdict {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Closed record-status vocabulary spelled exactly as the frozen schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditRecordStatus {
    /// Fully accounted by verified destinations.
    Conserved,
    /// Carries substantial unmatched residue.
    UnmatchedSubstantial,
    /// Line-edge-equivalent rewrite of its paired context.
    FormattingOnly,
}

impl AuditRecordStatus {
    /// Returns the frozen lowercase wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conserved => "conserved",
            Self::UnmatchedSubstantial => "unmatched_substantial",
            Self::FormattingOnly => "formatting_only",
        }
    }

    const fn from_engine(status: EngineStatus) -> Self {
        match status {
            EngineStatus::Conserved => Self::Conserved,
            EngineStatus::UnmatchedSubstantial => Self::UnmatchedSubstantial,
            EngineStatus::FormattingOnly => Self::FormattingOnly,
        }
    }
}

impl Serialize for AuditRecordStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Closed inconclusive-reason vocabulary spelled exactly as the six frozen
/// §9.3 reason strings.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AuditInconclusiveReason {
    /// Extraction or matching ran out of work units.
    WorkBudgetExceeded,
    /// Index memory failed after matching had begun.
    MemoryBudgetExceeded,
    /// One or more input files were skipped during ingestion.
    UnreadableInputSkipped,
    /// Multiplicity-counted matches exist under strict-uniqueness mode.
    StrictUniquenessAmbiguity,
    /// A region had no cold index window anywhere in the head tree.
    AmbiguousCommon,
    /// Serialized-report overflow dropped unmatched/formatting listings.
    ReportTruncated,
}

impl AuditInconclusiveReason {
    /// Returns the exact frozen §9.3 reason string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkBudgetExceeded => "work_budget_exceeded",
            Self::MemoryBudgetExceeded => "memory_budget_exceeded",
            Self::UnreadableInputSkipped => "unreadable_input_skipped",
            Self::StrictUniquenessAmbiguity => "strict_uniqueness_ambiguity",
            Self::AmbiguousCommon => "ambiguous_common",
            Self::ReportTruncated => "report_truncated",
        }
    }
}

impl From<EngineReason> for AuditInconclusiveReason {
    fn from(reason: EngineReason) -> Self {
        match reason {
            EngineReason::WorkBudgetExceeded => Self::WorkBudgetExceeded,
            EngineReason::MemoryBudgetExceeded => Self::MemoryBudgetExceeded,
            EngineReason::UnreadableInputSkipped => Self::UnreadableInputSkipped,
            EngineReason::StrictUniquenessAmbiguity => Self::StrictUniquenessAmbiguity,
            EngineReason::AmbiguousCommon => Self::AmbiguousCommon,
            EngineReason::ReportTruncated => Self::ReportTruncated,
        }
    }
}

impl Serialize for AuditInconclusiveReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Closed input-tree kind vocabulary spelled exactly as the frozen schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditInputKind {
    /// A recursively read directory tree.
    Directory,
    /// A `@MANIFEST`-named subset of paths.
    Manifest,
}

impl AuditInputKind {
    /// Returns the frozen lowercase wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Manifest => "manifest",
        }
    }
}

impl Serialize for AuditInputKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// One audited input tree summary mirrored into the report's `inputs` object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputTreeSummary {
    kind: AuditInputKind,
    files: u64,
    bytes: u64,
}

impl InputTreeSummary {
    /// Creates a summary from trusted ingestion counts.
    #[must_use]
    pub const fn new(kind: AuditInputKind, files: u64, bytes: u64) -> Self {
        Self { kind, files, bytes }
    }

    /// Returns the ingestion form of the tree.
    #[must_use]
    pub const fn kind(&self) -> AuditInputKind {
        self.kind
    }

    /// Returns the number of ingested regular files.
    #[must_use]
    pub const fn files(&self) -> u64 {
        self.files
    }

    /// Returns the aggregate ingested byte count.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// One skipped input path whose presence forces the fatal
/// `unreadable_input_skipped` reason and an `inconclusive` verdict (§7, §9.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkipInfo {
    path: String,
}

impl SkipInfo {
    /// Creates a skip record from one skipped normalized relative path.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the skipped normalized relative path spelling.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Lowerable rendering caps for the serialization budget (§11.3).
///
/// Fields stay private behind a trusted constructor, mirroring the house
/// `SnapshotLimits` / `RequestLimits` style: release values come from
/// [`Default`], and trusted tests may construct arbitrary lower (or boundary)
/// values. User-facing configuration may lower limits but never raise them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuditRenderLimits {
    regions_max: usize,
    unmatched_max: usize,
    formatting_max: usize,
    new_content_max: usize,
    serialized_bytes_max: u64,
}

impl AuditRenderLimits {
    /// Creates a rendering-limit set for trusted configurations and boundary
    /// tests.
    #[must_use]
    pub const fn new(
        regions_max: usize,
        unmatched_max: usize,
        formatting_max: usize,
        new_content_max: usize,
        serialized_bytes_max: u64,
    ) -> Self {
        Self {
            regions_max,
            unmatched_max,
            formatting_max,
            new_content_max,
            serialized_bytes_max,
        }
    }

    /// Returns the maximum number of listed conserved regions.
    #[must_use]
    pub const fn regions_max(&self) -> usize {
        self.regions_max
    }

    /// Returns the maximum number of listed unmatched-substantial entries.
    #[must_use]
    pub const fn unmatched_max(&self) -> usize {
        self.unmatched_max
    }

    /// Returns the maximum number of listed formatting-only entries.
    #[must_use]
    pub const fn formatting_max(&self) -> usize {
        self.formatting_max
    }

    /// Returns the maximum number of listed new-content spans.
    #[must_use]
    pub const fn new_content_max(&self) -> usize {
        self.new_content_max
    }

    /// Returns the serialized-report byte budget, trailing LF included.
    #[must_use]
    pub const fn serialized_bytes_max(&self) -> u64 {
        self.serialized_bytes_max
    }
}

impl Default for AuditRenderLimits {
    fn default() -> Self {
        Self::new(
            MAX_LISTED_REGIONS,
            MAX_UNMATCHED_LISTINGS,
            MAX_FORMATTING_LISTINGS,
            MAX_NEW_CONTENT_SPANS,
            MAX_SERIALIZED_REPORT_BYTES,
        )
    }
}

/// Constant empty `warnings` array pinned by the frozen v1 contract.
///
/// v1 defines no warning codes (§12), so the wire value is always `[]`; any
/// future warning is a wire-visible change requiring a version bump (§13).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AuditNoWarnings;

impl Serialize for AuditNoWarnings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let sequence = serializer.serialize_seq(Some(0))?;
        sequence.end()
    }
}

#[derive(Debug, Serialize)]
struct AuditOptionsDto {
    min_substantial_bytes: u64,
    min_fragment_bytes: u64,
    strict_uniqueness: bool,
    count_formatting: bool,
}

#[derive(Debug, Serialize)]
struct InputTreeDto {
    kind: AuditInputKind,
    files: u64,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct AuditInputsDto {
    base: InputTreeDto,
    head: InputTreeDto,
}

#[derive(Debug, Serialize)]
struct AuditTotalsDto {
    removed_regions: u64,
    removed_bytes: u64,
    conserved_bytes: u64,
    conserved_exact_bytes: u64,
    conserved_fragments_bytes: u64,
    formatting_only_bytes: u64,
    insubstantial_residue_bytes: u64,
    unmatched_substantial_bytes: u64,
    ambiguous_common_bytes: u64,
    unanalyzed_bytes: u64,
    new_bytes: u64,
    pairing_ambiguous_regions: u64,
    ambiguous_common_regions: u64,
}

#[derive(Debug, Serialize)]
struct DestinationDto {
    path: String,
    byte_range: (u64, u64),
    line_range: Option<(u64, u64)>,
}

#[derive(Debug, Serialize)]
struct PairedDestinationDto {
    path: String,
    byte_range: (u64, u64),
}

#[derive(Debug, Serialize)]
struct ConservedRegionDto {
    id: u64,
    source_path: String,
    source_byte_range: (u64, u64),
    source_line_range: Option<(u64, u64)>,
    length: u64,
    sha256: String,
    status: AuditRecordStatus,
    pairing_ambiguous: bool,
    destinations: Vec<DestinationDto>,
}

#[derive(Debug, Serialize)]
struct UnmatchedRegionDto {
    id: u64,
    source_path: String,
    source_byte_range: (u64, u64),
    source_line_range: Option<(u64, u64)>,
    length: u64,
    sha256: String,
    status: AuditRecordStatus,
}

#[derive(Debug, Serialize)]
struct FormattingRegionDto {
    id: u64,
    source_path: String,
    source_byte_range: (u64, u64),
    source_line_range: Option<(u64, u64)>,
    length: u64,
    sha256: String,
    status: AuditRecordStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    paired_destination: Option<PairedDestinationDto>,
}

#[derive(Debug, Serialize)]
struct NewSpanDto {
    path: String,
    byte_range: (u64, u64),
    line_range: Option<(u64, u64)>,
    length: u64,
    sha256: String,
}

/// Complete bounded `conservation-audit-v1` report ready for wire emission.
///
/// Construct exclusively through [`build_audit_report`] or
/// [`build_audit_report_with_limits`]; every field stays private so the wire
/// shape cannot drift from the frozen schema or the §11.3 policy table.
#[derive(Debug, Serialize)]
pub struct AuditReport {
    conservation_audit_version: u64,
    verdict: AuditVerdict,
    inconclusive_reasons: Vec<AuditInconclusiveReason>,
    options: AuditOptionsDto,
    inputs: AuditInputsDto,
    totals: AuditTotalsDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    omitted_region_count: Option<u64>,
    regions: Vec<ConservedRegionDto>,
    unmatched: Vec<UnmatchedRegionDto>,
    formatting: Vec<FormattingRegionDto>,
    new_content: Vec<NewSpanDto>,
    warnings: AuditNoWarnings,
}

/// Converts a completed engine outcome into a bounded audit report under the
/// release rendering limits.
///
/// # Errors
///
/// Returns `RESOURCE_LIMIT_EXCEEDED` when even the reserved skeleton exceeds
/// the serialized byte budget, and `INTERNAL_ERROR` when an engine-provided
/// record violates a documented invariant (ledger identity, range widths, or
/// ordering), so a bug fails loudly instead of emitting a wrong report.
pub fn build_audit_report(
    outcome: &ConservationOutcome,
    options_echo: &ConservationOptions,
    inputs: (InputTreeSummary, InputTreeSummary),
    skipped: &[SkipInfo],
    head_files: &[AuditFile<'_>],
) -> Result<AuditReport, ProtocolError> {
    build_audit_report_with_limits(
        outcome,
        options_echo,
        inputs,
        skipped,
        head_files,
        AuditRenderLimits::default(),
    )
}

/// Converts a completed engine outcome into a bounded audit report under the
/// supplied rendering limits (§11.3 policy table).
///
/// # Errors
///
/// Identical to [`build_audit_report`].
pub fn build_audit_report_with_limits(
    outcome: &ConservationOutcome,
    options_echo: &ConservationOptions,
    inputs: (InputTreeSummary, InputTreeSummary),
    skipped: &[SkipInfo],
    head_files: &[AuditFile<'_>],
    limits: AuditRenderLimits,
) -> Result<AuditReport, ProtocolError> {
    enforce_ledger_identity(&outcome.totals)?;

    let mut conserved: Vec<ConservedRegionDto> = Vec::new();
    let mut unmatched: Vec<UnmatchedRegionDto> = Vec::new();
    let mut formatting: Vec<FormattingRegionDto> = Vec::new();
    for record in &outcome.records {
        let common = convert_source_fields(record)?;
        match record.status {
            EngineStatus::Conserved => {
                let destinations = convert_destinations(&record.destinations)?;
                if destinations.is_empty() {
                    return Err(internal_error("conserved_region_without_destination"));
                }
                conserved.push(ConservedRegionDto {
                    id: common.id,
                    source_path: common.source_path,
                    source_byte_range: common.source_byte_range,
                    source_line_range: common.source_line_range,
                    length: common.length,
                    sha256: common.sha256,
                    status: AuditRecordStatus::from_engine(record.status),
                    pairing_ambiguous: record.pairing_ambiguous,
                    destinations,
                });
            }
            EngineStatus::UnmatchedSubstantial => unmatched.push(UnmatchedRegionDto {
                id: common.id,
                source_path: common.source_path,
                source_byte_range: common.source_byte_range,
                source_line_range: common.source_line_range,
                length: common.length,
                sha256: common.sha256,
                status: AuditRecordStatus::from_engine(record.status),
            }),
            EngineStatus::FormattingOnly => {
                // A paired insertion context can legitimately be zero-width
                // (a pure EOL conversion leaves an empty after-side
                // remainder); the wire range is then a `[n, n)` point.
                let paired_destination = record
                    .paired_context
                    .as_ref()
                    .map(|context| -> Result<PairedDestinationDto, ProtocolError> {
                        Ok(PairedDestinationDto {
                            path: path_string(&context.path)?,
                            byte_range: context.byte_range,
                        })
                    })
                    .transpose()?;
                formatting.push(FormattingRegionDto {
                    id: common.id,
                    source_path: common.source_path,
                    source_byte_range: common.source_byte_range,
                    source_line_range: common.source_line_range,
                    length: common.length,
                    sha256: common.sha256,
                    status: AuditRecordStatus::from_engine(record.status),
                    paired_destination,
                });
            }
        }
    }

    // Listings are ordered length-descending with stable source/id tie-breaks
    // (§11.1 ordering rules); the largest entries therefore sit at the front
    // and the lowest-priority entries at the back of each vector.
    conserved.sort_by(|left, right| right.length.cmp(&left.length).then(left.id.cmp(&right.id)));
    unmatched.sort_by(|left, right| right.length.cmp(&left.length).then(left.id.cmp(&right.id)));
    formatting.sort_by(|left, right| right.length.cmp(&left.length).then(left.id.cmp(&right.id)));

    let omitted_region_count = apply_entry_cap(conserved.len(), limits.regions_max(), |kept| {
        conserved.truncate(kept);
    });
    let unmatched_dropped = apply_entry_cap(unmatched.len(), limits.unmatched_max(), |kept| {
        unmatched.truncate(kept);
    });
    let formatting_dropped = apply_entry_cap(formatting.len(), limits.formatting_max(), |kept| {
        formatting.truncate(kept);
    });

    let mut new_content = Vec::with_capacity(outcome.new_content.len());
    let mut span_lines = SpanLineNumberer::new(head_files);
    for span in &outcome.new_content {
        new_content.push(convert_new_span(span, &mut span_lines)?);
    }
    new_content.sort_by(|left, right| {
        right
            .length
            .cmp(&left.length)
            .then(left.path.cmp(&right.path))
            .then(left.byte_range.cmp(&right.byte_range))
    });
    apply_entry_cap(new_content.len(), limits.new_content_max(), |kept| {
        new_content.truncate(kept);
    });

    let mut reasons: Vec<AuditInconclusiveReason> =
        outcome.reasons.iter().copied().map(Into::into).collect();
    if !skipped.is_empty() && !reasons.contains(&AuditInconclusiveReason::UnreadableInputSkipped) {
        reasons.push(AuditInconclusiveReason::UnreadableInputSkipped);
    }
    if unmatched_dropped > 0 || formatting_dropped > 0 {
        reasons.push(AuditInconclusiveReason::ReportTruncated);
    }
    // Fatal conditions dominate the engine verdict (§9.1): any reason forces
    // `inconclusive`, otherwise the engine semantics stand unchanged.
    let verdict = if reasons.is_empty() {
        AuditVerdict::from_engine(outcome.verdict)
    } else {
        AuditVerdict::Inconclusive
    };

    let mut report = AuditReport {
        conservation_audit_version: CONSERVATION_AUDIT_VERSION,
        verdict,
        inconclusive_reasons: reasons,
        options: AuditOptionsDto {
            min_substantial_bytes: options_echo.min_substantial_bytes(),
            min_fragment_bytes: options_echo.min_fragment_bytes(),
            strict_uniqueness: options_echo.strict_uniqueness(),
            count_formatting: options_echo.count_formatting(),
        },
        inputs: AuditInputsDto {
            base: InputTreeDto {
                kind: inputs.0.kind,
                files: inputs.0.files,
                bytes: inputs.0.bytes,
            },
            head: InputTreeDto {
                kind: inputs.1.kind,
                files: inputs.1.files,
                bytes: inputs.1.bytes,
            },
        },
        totals: convert_totals(&outcome.totals),
        omitted_region_count: (omitted_region_count > 0).then_some(omitted_region_count),
        regions: conserved,
        unmatched,
        formatting,
        new_content,
        warnings: AuditNoWarnings,
    };

    enforce_byte_budget(&mut report, limits.serialized_bytes_max())?;

    // Canonical reason order mirrors the engine's deduplicated ordering; the
    // permutation does not change the serialized length, so the byte budget
    // settled above remains satisfied.
    report.inconclusive_reasons.sort_unstable();
    report.inconclusive_reasons.dedup();

    enforce_listing_disjointness(&report)?;
    Ok(report)
}

/// Serializes one report as exactly one UTF-8 JSON value plus LF.
///
/// The caller must have produced the report through [`build_audit_report`] or
/// [`build_audit_report_with_limits`] so the serialized size already honors
/// the §11.3 byte budget.
///
/// # Errors
///
/// Returns `INTERNAL_ERROR` if serialization unexpectedly fails.
pub fn audit_report_json_line(report: &AuditReport) -> Result<String, ProtocolError> {
    to_json_line(report)
}

/// Shared identity fields of every reviewer-facing region record.
struct SourceFields {
    id: u64,
    source_path: String,
    source_byte_range: (u64, u64),
    source_line_range: Option<(u64, u64)>,
    length: u64,
    sha256: String,
}

fn convert_source_fields(record: &RegionRecord) -> Result<SourceFields, ProtocolError> {
    let (start, end) = record.source_byte_range;
    if end <= start || record.length == 0 || end - start != record.length {
        return Err(internal_error("region_length_mismatch"));
    }
    Ok(SourceFields {
        id: record.id,
        source_path: path_string(&record.source_path)?,
        source_byte_range: record.source_byte_range,
        source_line_range: record.source_line_range,
        length: record.length,
        sha256: record.sha256.to_prefixed_hex(),
    })
}

fn convert_destinations(
    destinations: &[srcmv_core::DestinationRecord],
) -> Result<Vec<DestinationDto>, ProtocolError> {
    for pair in destinations.windows(2) {
        let ordered = pair[0].path.cmp(&pair[1].path) == Ordering::Less
            || (pair[0].path == pair[1].path && pair[0].byte_range < pair[1].byte_range);
        if !ordered {
            return Err(internal_error("unordered_destinations"));
        }
    }
    destinations
        .iter()
        .map(|destination| {
            if destination.byte_range.1 <= destination.byte_range.0 {
                return Err(internal_error("empty_destination_range"));
            }
            Ok(DestinationDto {
                path: path_string(&destination.path)?,
                byte_range: destination.byte_range,
                line_range: destination.line_range,
            })
        })
        .collect()
}

fn convert_new_span(
    span: &srcmv_core::NewContentSpan,
    lines: &mut SpanLineNumberer<'_>,
) -> Result<NewSpanDto, ProtocolError> {
    let (start, end) = span.byte_range;
    if end <= start || span.length == 0 || span.length > end - start {
        return Err(internal_error("new_span_length_mismatch"));
    }
    let line_range = lines.line_range(&span.path, start, end)?;
    Ok(NewSpanDto {
        path: path_string(&span.path)?,
        byte_range: span.byte_range,
        line_range,
        length: span.length,
        sha256: span.sha256.to_prefixed_hex(),
    })
}

fn convert_totals(totals: &EngineTotals) -> AuditTotalsDto {
    AuditTotalsDto {
        removed_regions: totals.removed_regions,
        removed_bytes: totals.removed_bytes,
        conserved_bytes: totals.conserved_bytes,
        conserved_exact_bytes: totals.conserved_exact_bytes,
        conserved_fragments_bytes: totals.conserved_fragments_bytes,
        formatting_only_bytes: totals.formatting_only_bytes,
        insubstantial_residue_bytes: totals.insubstantial_residue_bytes,
        unmatched_substantial_bytes: totals.unmatched_substantial_bytes,
        ambiguous_common_bytes: totals.ambiguous_common_bytes,
        unanalyzed_bytes: totals.unanalyzed_bytes,
        new_bytes: totals.new_bytes,
        pairing_ambiguous_regions: totals.pairing_ambiguous_regions,
        ambiguous_common_regions: totals.ambiguous_common_regions,
    }
}

/// Applies one structural entry cap, returning how many lowest-priority
/// entries the truncation dropped.
fn apply_entry_cap(length: usize, maximum: usize, truncate: impl FnOnce(usize)) -> u64 {
    if length <= maximum {
        return 0;
    }
    let dropped = u64::try_from(length - maximum).unwrap_or(u64::MAX);
    truncate(maximum);
    dropped
}

/// Drops lowest-priority listings under a running byte budget (§11.3): any
/// listing pop this budget performs — conserved regions first (silent,
/// counted via `omitted_region_count`), then new-content spans, and only
/// then unmatched/formatting entries — marks every non-regions drop with the
/// fatal `report_truncated` reason and forces [`Verdict::Inconclusive`],
/// because §9.3 keys the reason on "the serialized report overflowed its
/// byte cap AND listings were dropped", not on which cap fired. This is
/// deliberately asymmetric with the structural §10 caps applied through
/// [`apply_entry_cap`]: the §11.3 policy table assigns no consequence to the
/// new-content span cap, so a structural drop there stays silent, while the
/// structural regions row stays silent plus `omitted_region_count` and the
/// structural unmatched/formatting rows are fatal exactly like their
/// budget-driven counterparts. Never drops totals, verdict, reasons,
/// options, or inputs; errors when even the reserved skeleton cannot fit.
fn enforce_byte_budget(report: &mut AuditReport, budget: u64) -> Result<(), ProtocolError> {
    loop {
        let required = measured_payload_bytes(report)?.saturating_add(1);
        if required <= budget {
            return Ok(());
        }
        if report.regions.pop().is_some() {
            let updated = report.omitted_region_count.unwrap_or(0).saturating_add(1);
            report.omitted_region_count = Some(updated);
        } else if report.new_content.pop().is_some()
            || report.unmatched.pop().is_some()
            || report.formatting.pop().is_some()
        {
            mark_report_truncated(report);
        } else {
            return Err(limit_envelope(required, budget));
        }
    }
}

fn mark_report_truncated(report: &mut AuditReport) {
    if !report
        .inconclusive_reasons
        .contains(&AuditInconclusiveReason::ReportTruncated)
    {
        report
            .inconclusive_reasons
            .push(AuditInconclusiveReason::ReportTruncated);
    }
    report.verdict = AuditVerdict::Inconclusive;
}

fn measured_payload_bytes(report: &AuditReport) -> Result<u64, ProtocolError> {
    let length = serde_json::to_string(report)
        .map_err(|_| internal_error("serialized_report_measurement_failed"))?
        .len();
    u64::try_from(length).map_err(|_| internal_error("serialized_report_length_overflow"))
}

/// Recomputes the §9.2 ledger identity so a bookkeeping bug fails loudly.
fn enforce_ledger_identity(totals: &EngineTotals) -> Result<(), ProtocolError> {
    let conserved = totals
        .conserved_exact_bytes
        .checked_add(totals.conserved_fragments_bytes)
        .ok_or_else(|| internal_error("ledger_identity_overflow"))?;
    let partial = conserved
        .checked_add(totals.unmatched_substantial_bytes)
        .and_then(|sum| sum.checked_add(totals.insubstantial_residue_bytes))
        .and_then(|sum| sum.checked_add(totals.formatting_only_bytes))
        .and_then(|sum| sum.checked_add(totals.ambiguous_common_bytes))
        .and_then(|sum| sum.checked_add(totals.unanalyzed_bytes))
        .ok_or_else(|| internal_error("ledger_identity_overflow"))?;
    if partial != totals.removed_bytes || conserved != totals.conserved_bytes {
        return Err(internal_error("ledger_identity_violated"));
    }
    Ok(())
}

/// Recomputes the §8.8/§9.2 report-level guarantee from the assembled
/// listings: destination intervals are pairwise disjoint per head path,
/// new-content spans are pairwise disjoint per head path, and every span's
/// surviving length equals its width minus the consumed bytes inside it
/// (`consumed ∪ surviving_new` therefore never overlaps).
fn enforce_listing_disjointness(report: &AuditReport) -> Result<(), ProtocolError> {
    let mut consumed: BTreeMap<&str, Vec<(u64, u64)>> = BTreeMap::new();
    for region in &report.regions {
        for destination in &region.destinations {
            if !insert_disjoint(&mut consumed, &destination.path, destination.byte_range) {
                return Err(internal_error("overlapping_consumed_interval"));
            }
        }
    }
    let mut fresh: BTreeMap<&str, Vec<(u64, u64)>> = BTreeMap::new();
    for span in &report.new_content {
        if !insert_disjoint(&mut fresh, &span.path, span.byte_range) {
            return Err(internal_error("overlapping_new_content_span"));
        }
        let (start, end) = span.byte_range;
        let covered_inside = consumed
            .get(span.path.as_str())
            .map(|intervals| {
                intervals
                    .iter()
                    .map(|&(low, high)| high.min(end).saturating_sub(low.max(start)))
                    .fold(0_u64, |sum, overlap| sum.saturating_add(overlap))
            })
            .unwrap_or(0);
        if covered_inside != end - start - span.length {
            return Err(internal_error("new_span_survivor_mismatch"));
        }
    }
    Ok(())
}

fn insert_disjoint<'a>(
    per_path: &mut BTreeMap<&'a str, Vec<(u64, u64)>>,
    path: &'a str,
    interval: (u64, u64),
) -> bool {
    if interval.1 <= interval.0 {
        return false;
    }
    let intervals = per_path.entry(path).or_default();
    let disjoint = intervals
        .iter()
        .all(|&(low, high)| interval.0 >= high || interval.1 <= low);
    if !disjoint {
        return false;
    }
    intervals.push(interval);
    true
}

/// Lazily numbers new-content spans against their hosting head files,
/// skipping binary and empty files exactly like the engine's presentation
/// layer (§9.5). Text files whose [`LineIndex`] construction breaches the
/// house caps (`SPAN_LINE_INDEX_MAX_LINES` / `SPAN_LINE_INDEX_MEMORY_BYTES`,
/// i.e. more than five million lines within the 256 MiB ingestion bound)
/// present `line_range` null like binary files — a presentation-only
/// degradation that stays deterministic.
struct SpanLineNumberer<'a> {
    head_files: &'a [AuditFile<'a>],
    rank: HashMap<Vec<u8>, usize>,
    indexes: HashMap<usize, Option<LineIndex>>,
}

impl<'a> SpanLineNumberer<'a> {
    fn new(head_files: &'a [AuditFile<'a>]) -> Self {
        Self {
            head_files,
            rank: head_files
                .iter()
                .enumerate()
                .map(|(index, file)| (file.path.to_vec(), index))
                .collect(),
            indexes: HashMap::new(),
        }
    }

    fn line_range(
        &mut self,
        path: &[u8],
        start: u64,
        end: u64,
    ) -> Result<Option<(u64, u64)>, ProtocolError> {
        let file_index = *self
            .rank
            .get(path)
            .ok_or_else(|| internal_error("audit_head_file_missing"))?;
        let cached = self
            .indexes
            .entry(file_index)
            .or_insert_with(|| {
                let file = &self.head_files[file_index];
                if file.is_binary() || file.bytes.is_empty() {
                    None
                } else {
                    LineIndex::from_bytes_with_limits(
                        file.bytes,
                        SPAN_LINE_INDEX_MAX_LINES,
                        SPAN_LINE_INDEX_MEMORY_BYTES,
                    )
                    .ok()
                }
            })
            .as_ref();
        let Some(index) = cached else {
            return Ok(None);
        };
        let count = index.line_count();
        if end <= start || count == 0 {
            return Ok(None);
        }
        let first = last_line_with_start_at_or_before(index, count, start);
        let last = last_line_with_start_before(index, count, end);
        Ok((last > 0).then_some((first, last)))
    }
}

fn last_line_with_start_at_or_before(index: &LineIndex, count: u64, offset: u64) -> u64 {
    let mut low = 1_u64;
    let mut high = count;
    let mut answer = 1_u64;
    while low <= high {
        let middle = low + (high - low) / 2;
        if index.line_start(middle).unwrap_or(u64::MAX) <= offset {
            answer = middle;
            low = middle + 1;
        } else {
            high = middle.saturating_sub(1);
        }
    }
    answer
}

fn last_line_with_start_before(index: &LineIndex, count: u64, bound: u64) -> u64 {
    let mut low = 1_u64;
    let mut high = count;
    let mut answer = 0_u64;
    while low <= high {
        let middle = low + (high - low) / 2;
        if index.line_start(middle).unwrap_or(u64::MAX) < bound {
            answer = middle;
            low = middle + 1;
        } else {
            high = middle.saturating_sub(1);
        }
    }
    answer
}

fn path_string(path: &[u8]) -> Result<String, ProtocolError> {
    String::from_utf8(path.to_vec()).map_err(|_| internal_error("non_utf8_audit_path"))
}

fn audit_context<const N: usize>(
    entries: [(&'static str, serde_json::Value); N],
) -> BTreeMap<String, serde_json::Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn internal_error(invariant: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorDto::new(
        ErrorCode::InternalError,
        "an internal conservation-audit invariant failed",
        audit_context([("invariant", serde_json::json!(invariant))]),
    ))
}

fn limit_envelope(actual: u64, limit: u64) -> ProtocolError {
    ProtocolError::new(ErrorDto::new(
        ErrorCode::ResourceLimitExceeded,
        "the serialized conservation-audit report exceeds the response limit",
        audit_context([
            ("actual", serde_json::json!(actual)),
            ("limit", serde_json::json!(limit)),
            ("resource", serde_json::json!("serialized_audit_report")),
        ]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use jsonschema::{Registry, Validator};

    use crate::ErrorCode;
    use srcmv_core::{DestinationRecord, NewContentSpan, Sha256Digest};

    const REPORT_SCHEMA_ID: &str = "https://raw.githubusercontent.com/atacan/srcmv/main/docs/schema/conservation-audit-v1/report.schema.json";

    /// Compiles the frozen schema offline exactly like `audit_v1.rs` so
    /// builder-level tests can pin wire shapes against the real contract.
    fn report_validator() -> Validator {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/schema/conservation-audit-v1/report.schema.json");
        let bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
        let schema: serde_json::Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("report schema must contain JSON: {error}"));
        let registry = Registry::new()
            .add(REPORT_SCHEMA_ID, schema.clone())
            .expect("report schema ID must be a valid URI")
            .prepare()
            .expect("offline schema registry must prepare");
        jsonschema::draft202012::options()
            .with_registry(&registry)
            .build(&schema)
            .expect("report schema must compile")
    }

    const DEFAULT_LIMITS: AuditRenderLimits = AuditRenderLimits::new(
        MAX_LISTED_REGIONS,
        MAX_UNMATCHED_LISTINGS,
        MAX_FORMATTING_LISTINGS,
        MAX_NEW_CONTENT_SPANS,
        MAX_SERIALIZED_REPORT_BYTES,
    );

    fn options() -> ConservationOptions {
        ConservationOptions::default()
    }

    fn strict_options() -> ConservationOptions {
        ConservationOptions::new(32, 16, true, false).expect("strict options should validate")
    }

    fn summaries() -> (InputTreeSummary, InputTreeSummary) {
        (
            InputTreeSummary::new(AuditInputKind::Directory, 2, 100),
            InputTreeSummary::new(AuditInputKind::Directory, 2, 120),
        )
    }

    fn head_files() -> Vec<AuditFile<'static>> {
        let leaked: &'static [u8] =
            Box::leak(b"created padding content\n".to_vec().into_boxed_slice());
        let digest: &'static Sha256Digest = Box::leak(Box::new(Sha256Digest([7; 32])));
        vec![AuditFile {
            path: Box::leak(b"dst/new.txt".to_vec().into_boxed_slice()),
            bytes: leaked,
            digest,
        }]
    }

    fn destination(path: &[u8], start: u64, end: u64) -> DestinationRecord {
        DestinationRecord {
            path: path.to_vec(),
            byte_range: (start, end),
            line_range: Some((1, 2)),
            sha256: Sha256Digest([9; 32]),
        }
    }

    fn conserved_record(id: u64, length: u64) -> RegionRecord {
        RegionRecord {
            id,
            source_path: b"src/gone.txt".to_vec(),
            source_byte_range: (0, length),
            source_line_range: Some((1, 1)),
            length,
            sha256: Sha256Digest([id as u8; 32]),
            status: srcmv_core::RegionStatus::Conserved,
            pairing_ambiguous: false,
            destinations: vec![destination(
                b"dst/kept.txt",
                id * 10_000,
                id * 10_000 + length,
            )],
            paired_context: None,
        }
    }

    fn unmatched_record(id: u64, length: u64) -> RegionRecord {
        RegionRecord {
            status: srcmv_core::RegionStatus::UnmatchedSubstantial,
            destinations: Vec::new(),
            ..conserved_record(id, length)
        }
    }

    fn formatting_record(id: u64, length: u64) -> RegionRecord {
        RegionRecord {
            status: srcmv_core::RegionStatus::FormattingOnly,
            paired_context: Some(srcmv_core::PairedDestination {
                path: b"src/gone.txt".to_vec(),
                byte_range: (0, length),
            }),
            ..conserved_record(id, length)
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn totals(
        removed_regions: u64,
        exact: u64,
        fragments: u64,
        insubstantial: u64,
        unmatched: u64,
        formatting: u64,
        ambiguous: u64,
        unanalyzed: u64,
        new_bytes: u64,
        pairing_ambiguous_regions: u64,
    ) -> EngineTotals {
        let conserved = exact + fragments;
        EngineTotals {
            removed_regions,
            removed_bytes: conserved
                + insubstantial
                + unmatched
                + formatting
                + ambiguous
                + unanalyzed,
            conserved_bytes: conserved,
            conserved_exact_bytes: exact,
            conserved_fragments_bytes: fragments,
            formatting_only_bytes: formatting,
            insubstantial_residue_bytes: insubstantial,
            unmatched_substantial_bytes: unmatched,
            ambiguous_common_bytes: ambiguous,
            unanalyzed_bytes: unanalyzed,
            new_bytes,
            pairing_ambiguous_regions,
            ambiguous_common_regions: 0,
        }
    }

    fn outcome(verdict: EngineVerdict, records: Vec<RegionRecord>) -> ConservationOutcome {
        let term = |status| {
            records
                .iter()
                .filter(|record| record.status == status)
                .map(|record| record.length)
                .sum()
        };
        ConservationOutcome {
            verdict,
            reasons: Vec::new(),
            totals: totals(
                records.len() as u64,
                term(srcmv_core::RegionStatus::Conserved),
                0,
                0,
                term(srcmv_core::RegionStatus::UnmatchedSubstantial),
                term(srcmv_core::RegionStatus::FormattingOnly),
                0,
                0,
                0,
                0,
            ),
            records,
            new_content: Vec::new(),
        }
    }

    fn build(outcome: &ConservationOutcome, limits: AuditRenderLimits) -> AuditReport {
        build_audit_report_with_limits(outcome, &options(), summaries(), &[], &head_files(), limits)
            .expect("prepared outcome should always render")
    }

    fn build_failing(outcome: &ConservationOutcome, limits: AuditRenderLimits) -> ProtocolError {
        build_audit_report_with_limits(outcome, &options(), summaries(), &[], &head_files(), limits)
            .expect_err("prepared outcome should always fail")
    }

    fn serialized(report: &AuditReport) -> serde_json::Value {
        serde_json::from_str(&serde_json::to_string(report).expect("report should serialize"))
            .expect("serialized report should parse")
    }

    fn array_length(value: &serde_json::Value, key: &str) -> Option<usize> {
        value[key].as_array().map(Vec::len)
    }

    #[test]
    fn render_limits_should_default_to_release_values() {
        let limits = AuditRenderLimits::default();
        assert_eq!(limits.regions_max(), 2_000);
        assert_eq!(limits.unmatched_max(), 200);
        assert_eq!(limits.formatting_max(), 200);
        assert_eq!(limits.new_content_max(), 50);
        assert_eq!(limits.serialized_bytes_max(), MAX_SERIALIZED_REPORT_BYTES);
    }

    #[test]
    fn build_report_should_echo_engine_verdict_options_and_totals_for_a_clean_pass() {
        let prepared = outcome(EngineVerdict::Pass, vec![conserved_record(0, 48)]);

        let json = serialized(&build(&prepared, DEFAULT_LIMITS));

        assert_eq!(json["conservation_audit_version"], 1);
        assert_eq!(json["verdict"], "pass");
        assert_eq!(json["inconclusive_reasons"], serde_json::json!([]));
        assert_eq!(json["options"]["min_substantial_bytes"], 32);
        assert_eq!(json["options"]["min_fragment_bytes"], 16);
        assert_eq!(json["options"]["strict_uniqueness"], false);
        assert_eq!(json["options"]["count_formatting"], false);
        assert_eq!(json["totals"]["removed_bytes"], 48);
        assert_eq!(json["totals"]["conserved_exact_bytes"], 48);
        assert_eq!(json["warnings"], serde_json::json!([]));
        assert!(json.get("omitted_region_count").is_none());
        assert_eq!(array_length(&json, "regions"), Some(1));
        assert_eq!(json["regions"][0]["status"], "conserved");
        assert_eq!(
            json["regions"][0]["sha256"],
            format!("sha256:{:064x}", 0_u64)
        );
        assert_eq!(json["inputs"]["base"]["kind"], "directory");
        assert_eq!(json["inputs"]["base"]["files"], 2);
        assert_eq!(
            json["regions"][0]["destinations"][0]["line_range"],
            serde_json::json!([1, 2])
        );
    }

    #[test]
    fn skips_should_force_inconclusive_and_dedupe_the_fatal_reason() {
        let passing = outcome(EngineVerdict::Pass, vec![conserved_record(0, 48)]);
        let failing = outcome(EngineVerdict::Fail, vec![unmatched_record(0, 64)]);
        let skips = vec![
            SkipInfo::new("gone/a.txt"),
            SkipInfo::new("gone/a.txt"),
            SkipInfo::new("gone/b.txt"),
        ];

        for (label, prepared) in [("pass", &passing), ("fail", &failing)] {
            let report = build_audit_report_with_limits(
                prepared,
                &options(),
                summaries(),
                &skips,
                &head_files(),
                DEFAULT_LIMITS,
            )
            .unwrap_or_else(|error| panic!("{label} report should build: {error}"));
            let json = serialized(&report);
            assert_eq!(json["verdict"], "inconclusive", "{label}");
            assert_eq!(
                json["inconclusive_reasons"],
                serde_json::json!(["unreadable_input_skipped"]),
                "{label} reasons must dedupe to one entry"
            );
        }
    }

    #[test]
    fn regions_cap_should_keep_largest_first_below_at_above_the_limit() {
        let records: Vec<RegionRecord> = (0..2_001)
            .map(|id| conserved_record(id, 1_000 + id))
            .collect();
        let prepared = outcome(EngineVerdict::Pass, records);

        let above = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_001, 200, 200, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&above, "regions"), Some(2_001));
        assert!(above.get("omitted_region_count").is_none());
        assert_eq!(above["verdict"], "pass");

        let at = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 200, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&at, "regions"), Some(2_000));
        assert_eq!(at["omitted_region_count"], 1);
        assert_eq!(at["verdict"], "pass", "region drops are not fatal");
        assert_eq!(at["regions"][0]["length"], 3_000, "largest first");

        let below = serialized(&build(
            &prepared,
            AuditRenderLimits::new(1_999, 200, 200, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&below, "regions"), Some(1_999));
        assert_eq!(below["omitted_region_count"], 2);

        for label in ["at", "below"] {
            let json = if label == "at" { &at } else { &below };
            let rows = json["regions"].as_array().expect("regions array");
            let lengths: Vec<u64> = rows
                .iter()
                .map(|row| row["length"].as_u64().expect("length"))
                .collect();
            let ids: Vec<u64> = rows
                .iter()
                .map(|row| row["id"].as_u64().expect("id"))
                .collect();
            assert!(
                lengths.windows(2).all(|pair| pair[0] >= pair[1]),
                "{label} lengths must run largest-first"
            );
            assert!(
                lengths
                    .windows(2)
                    .zip(ids.windows(2))
                    .all(|(lengths_pair, ids_pair)| {
                        lengths_pair[0] > lengths_pair[1] || ids_pair[0] < ids_pair[1]
                    }),
                "{label} ties must break by ascending id"
            );
        }
    }

    #[test]
    fn unmatched_cap_should_keep_largest_and_downgrade_even_an_engine_fail() {
        let records: Vec<RegionRecord> =
            (0..201).map(|id| unmatched_record(id, 500 + id)).collect();
        let prepared = outcome(EngineVerdict::Fail, records);

        let above = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 201, 200, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&above, "unmatched"), Some(201));
        assert_eq!(above["verdict"], "fail");
        assert_eq!(above["inconclusive_reasons"], serde_json::json!([]));

        let at = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 200, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&at, "unmatched"), Some(200));
        assert_eq!(at["unmatched"][0]["length"], 700, "largest kept first");
        assert_eq!(at["verdict"], "inconclusive", "one drop from 201 is fatal");
        assert_eq!(
            at["inconclusive_reasons"],
            serde_json::json!(["report_truncated"])
        );

        let below = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 199, 200, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&below, "unmatched"), Some(199));
        assert_eq!(below["verdict"], "inconclusive", "any drop is fatal");
        assert_eq!(
            below["inconclusive_reasons"],
            serde_json::json!(["report_truncated"])
        );
        assert_eq!(
            below["totals"]["unmatched_substantial_bytes"],
            prepared.totals.unmatched_substantial_bytes,
            "totals are never truncated"
        );
    }

    #[test]
    fn formatting_cap_should_behave_like_the_unmatched_row() {
        let records: Vec<RegionRecord> =
            (0..201).map(|id| formatting_record(id, 40 + id)).collect();
        let prepared = outcome(EngineVerdict::Pass, records);

        let exact = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 201, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(
            array_length(&exact, "formatting"),
            Some(201),
            "an exact fit keeps every entry"
        );
        assert_eq!(
            exact["verdict"], "pass",
            "no drop leaves the engine pass intact"
        );
        assert_eq!(exact["inconclusive_reasons"], serde_json::json!([]));

        let at = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 200, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&at, "formatting"), Some(200));
        assert_eq!(at["formatting"][0]["length"], 240);
        assert_eq!(at["formatting"][0]["status"], "formatting_only");
        assert!(at["formatting"][0].get("paired_destination").is_some());
        assert_eq!(at["verdict"], "inconclusive", "one drop from 201 is fatal");
        assert_eq!(
            at["inconclusive_reasons"],
            serde_json::json!(["report_truncated"])
        );

        let below = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 199, 50, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&below, "formatting"), Some(199));
        assert_eq!(below["verdict"], "inconclusive");
        assert_eq!(
            below["inconclusive_reasons"],
            serde_json::json!(["report_truncated"])
        );
    }

    #[test]
    fn new_content_cap_should_keep_largest_spans_below_at_above_the_limit() {
        let mut prepared = outcome(EngineVerdict::Pass, Vec::new());
        prepared.new_content = (0..60)
            .map(|index| NewContentSpan {
                path: b"dst/new.txt".to_vec(),
                byte_range: ((index + 1) * 10_000, (index + 1) * 10_000 + 1_000 + index),
                length: 1_000 + index,
                sha256: Sha256Digest([index as u8; 32]),
            })
            .collect();

        for (cap, expected) in [(49_usize, 49_usize), (50, 50), (51, 51)] {
            let json = serialized(&build(
                &prepared,
                AuditRenderLimits::new(2_000, 200, 200, cap, MAX_SERIALIZED_REPORT_BYTES),
            ));
            assert_eq!(array_length(&json, "new_content"), Some(expected));
            assert_eq!(
                json["new_content"][0]["length"], 1_059,
                "largest span first"
            );
            assert_eq!(
                json["verdict"], "pass",
                "new-content drops never affect the verdict"
            );
        }

        let exact = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 200, 60, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(
            array_length(&exact, "new_content"),
            Some(60),
            "an exact fit keeps every span"
        );
        assert_eq!(exact["verdict"], "pass");
        assert_eq!(exact["inconclusive_reasons"], serde_json::json!([]));
    }

    #[test]
    fn byte_budget_should_drop_listings_silently_before_any_fatality() {
        let prepared = outcome(
            EngineVerdict::Fail,
            vec![
                conserved_record(0, 900),
                conserved_record(1, 800),
                unmatched_record(2, 700),
            ],
        );

        let full = build(&prepared, DEFAULT_LIMITS);
        let full_bytes = measured_payload_bytes(&full).expect("measurement") + 1;

        let json = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 200, 50, full_bytes),
        ));
        assert_eq!(json["verdict"], "fail", "exact-fit budget drops nothing");
        assert_eq!(array_length(&json, "regions"), Some(2));
        assert_eq!(array_length(&json, "unmatched"), Some(1));
        assert!(json.get("omitted_region_count").is_none());

        // Prune the silent rows manually and measure: a budget at exactly that
        // size must reproduce the same state, proving silent rows drain before
        // any fatal row is touched.
        let mut pruned = build(&prepared, DEFAULT_LIMITS);
        pruned.regions.clear();
        pruned.new_content.clear();
        pruned.omitted_region_count = Some(2);
        let pruned_bytes = measured_payload_bytes(&pruned).expect("measurement") + 1;
        let json = serialized(&build(
            &prepared,
            AuditRenderLimits::new(0, 200, 200, 0, pruned_bytes),
        ));
        assert_eq!(array_length(&json, "regions"), Some(0));
        assert_eq!(array_length(&json, "new_content"), Some(0));
        assert_eq!(json["omitted_region_count"], 2);
        assert_eq!(
            array_length(&json, "unmatched"),
            Some(1),
            "fatal row survives silence"
        );
        assert_eq!(json["verdict"], "fail", "silent drops never downgrade");
        assert_eq!(json["inconclusive_reasons"], serde_json::json!([]));
    }

    #[test]
    fn byte_budget_exhausting_silent_rows_should_trip_the_fatal_truncation_row() {
        let prepared = outcome(EngineVerdict::Fail, vec![unmatched_record(0, 700)]);

        // Size of the fully-pruned report carrying the truncation reason and
        // its forced inconclusive verdict: exactly what the builder holds when
        // the fatal row drains last.
        let mut floor = build(&prepared, DEFAULT_LIMITS);
        floor.unmatched.clear();
        floor.inconclusive_reasons = vec![AuditInconclusiveReason::ReportTruncated];
        floor.verdict = AuditVerdict::Inconclusive;
        let floor_bytes = measured_payload_bytes(&floor).expect("measurement") + 1;

        let json = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 0, 200, 50, floor_bytes),
        ));
        assert_eq!(array_length(&json, "unmatched"), Some(0));
        assert_eq!(
            json["verdict"], "inconclusive",
            "truncation overrides an engine fail"
        );
        assert_eq!(
            json["inconclusive_reasons"],
            serde_json::json!(["report_truncated"])
        );
        assert_eq!(json["totals"]["unmatched_substantial_bytes"], 700);
    }

    #[test]
    fn structural_new_content_cap_should_stay_silent_while_a_byte_budget_pop_is_fatal() {
        let mut prepared = outcome(EngineVerdict::Pass, Vec::new());
        prepared.new_content = vec![NewContentSpan {
            path: b"dst/new.txt".to_vec(),
            byte_range: (10_000, 10_100),
            length: 100,
            sha256: Sha256Digest([3; 32]),
        }];

        // Structural §10 cap over one span: §11.3 assigns no consequence to
        // this row, so the drop is silent and the engine pass stands intact.
        let structural = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 200, 0, MAX_SERIALIZED_REPORT_BYTES),
        ));
        assert_eq!(array_length(&structural, "new_content"), Some(0));
        assert_eq!(
            structural["verdict"], "pass",
            "structural cap never downgrades"
        );
        assert_eq!(structural["inconclusive_reasons"], serde_json::json!([]));

        // The same span popped by the byte budget instead: §9.3 keys
        // `report_truncated` on overflow with dropped listings, so the
        // outcome is fatal regardless of which listing was sacrificed.
        let mut floor = build(&prepared, DEFAULT_LIMITS);
        floor.new_content.clear();
        floor.inconclusive_reasons = vec![AuditInconclusiveReason::ReportTruncated];
        floor.verdict = AuditVerdict::Inconclusive;
        let floor_bytes = measured_payload_bytes(&floor).expect("measurement") + 1;

        let budgeted = serialized(&build(
            &prepared,
            AuditRenderLimits::new(2_000, 200, 200, 50, floor_bytes),
        ));
        assert_eq!(array_length(&budgeted, "new_content"), Some(0));
        assert_eq!(
            budgeted["verdict"], "inconclusive",
            "a budget-driven span pop is fatal"
        );
        assert_eq!(
            budgeted["inconclusive_reasons"],
            serde_json::json!(["report_truncated"])
        );
    }

    #[test]
    fn byte_budget_below_the_skeleton_should_fail_closed_with_a_limit_envelope() {
        let prepared = outcome(EngineVerdict::Fail, vec![unmatched_record(0, 700)]);

        let error = build_failing(&prepared, AuditRenderLimits::new(2_000, 200, 200, 50, 8));
        assert_eq!(error.report().code(), ErrorCode::ResourceLimitExceeded);
        assert_eq!(
            error.report().context()["resource"],
            serde_json::json!("serialized_audit_report")
        );
        assert_eq!(error.report().exit_code(), 4);
    }

    #[test]
    fn ledger_identity_violation_should_fail_loudly_as_an_internal_error() {
        let mut prepared = outcome(EngineVerdict::Pass, vec![conserved_record(0, 48)]);
        prepared.totals.removed_bytes += 1;

        let error = build_failing(&prepared, DEFAULT_LIMITS);
        assert_eq!(error.report().code(), ErrorCode::InternalError);
    }

    #[test]
    fn strict_uniqueness_options_should_be_echoed_into_the_report() {
        let prepared = outcome(EngineVerdict::Pass, Vec::new());
        let report = build_audit_report_with_limits(
            &prepared,
            &strict_options(),
            summaries(),
            &[],
            &head_files(),
            DEFAULT_LIMITS,
        )
        .expect("echo build");
        let json = serialized(&report);
        assert_eq!(json["options"]["strict_uniqueness"], true);
        assert_eq!(json["options"]["count_formatting"], false);
    }

    #[test]
    fn serialization_should_be_deterministic_across_repeated_builds() {
        let prepared = outcome(
            EngineVerdict::Pass,
            vec![conserved_record(0, 48), formatting_record(1, 3)],
        );
        let first = audit_report_json_line(&build(&prepared, DEFAULT_LIMITS)).expect("first");
        let second = audit_report_json_line(&build(&prepared, DEFAULT_LIMITS)).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn binary_head_participation_should_serialize_null_line_ranges_that_validate() {
        let mut blob = vec![0_u8];
        blob.extend_from_slice(b"created head-side padding\n");
        blob.resize(192, 0xFF);
        let head = vec![AuditFile {
            path: Box::leak(b"dst/blob.bin".to_vec().into_boxed_slice()),
            bytes: Box::leak(blob.into_boxed_slice()),
            digest: Box::leak(Box::new(Sha256Digest([5; 32]))),
        }];
        let mut record = conserved_record(0, 48);
        record.destinations = vec![DestinationRecord {
            path: b"dst/blob.bin".to_vec(),
            byte_range: (0, 48),
            line_range: None,
            sha256: Sha256Digest([9; 32]),
        }];
        let mut prepared = outcome(EngineVerdict::Pass, vec![record]);
        prepared.new_content = vec![NewContentSpan {
            path: b"dst/blob.bin".to_vec(),
            byte_range: (64, 128),
            length: 64,
            sha256: Sha256Digest([6; 32]),
        }];

        let report = build_audit_report_with_limits(
            &prepared,
            &options(),
            summaries(),
            &[],
            &head,
            DEFAULT_LIMITS,
        )
        .expect("binary-head report should render");
        let json = serialized(&report);
        assert_eq!(
            json["regions"][0]["destinations"][0]["line_range"],
            serde_json::Value::Null,
            "a match landing in a binary head file carries no line range"
        );
        assert_eq!(
            json["new_content"][0]["line_range"],
            serde_json::Value::Null,
            "a new-content span inside a binary created file carries no line range"
        );

        report_validator()
            .validate(&serde_json::to_value(&report).expect("report should reparse"))
            .unwrap_or_else(|error| {
                panic!("wire-null report must satisfy the frozen schema: {error}")
            });
    }

    #[test]
    fn json_line_emission_should_produce_exactly_one_lf_terminated_value() {
        let prepared = outcome(EngineVerdict::Pass, vec![conserved_record(0, 48)]);
        let line = audit_report_json_line(&build(&prepared, DEFAULT_LIMITS)).expect("emission");

        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "exactly one trailing LF");
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("the line must be one JSON value");
        assert_eq!(parsed["conservation_audit_version"], 1);
    }
}
