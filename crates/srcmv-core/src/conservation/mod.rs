//! Byte-level conservation auditing engine.
//!
//! Pure realization of plan §8–§9: given two borrowed tree projections, the
//! engine proves at the byte level which removed content reappeared exactly
//! somewhere in the head tree, reports formatting-only rewrites and genuinely
//! new bytes separately, and fails closed to [`Verdict::Inconclusive`] with
//! machine-readable reasons whenever work is truncated or ambiguity blocks a
//! reliable conservation claim. Greedy matching never over-credits: every
//! conserved byte is byte-verified against an exclusively consumed head
//! interval, so the only incompleteness is conservative (§4).
//!
//! The planned `match.rs` module realizes as `matching.rs` (`match` is a
//! Rust keyword); that filename is the sanctioned deviation from §14.
//!
//! Post-matching assembly digests — whole-file digests synthesized for
//! unanalyzed placeholders and digests over new-content spans — are
//! deliberately outside the extraction and matching work-unit pools: they are
//! deterministic, bounded by the retained-region cap here and by the report
//! caps arriving in Phase 3, and they run only after all verdict-relevant
//! work has completed.
//!
//! The engine performs zero syscalls: inputs are borrowed slices only, there
//! are no clocks or randomness sources, identical inputs produce identical
//! outcomes, and no request, journal, plan-hash, identity-hash, or log type
//! appears here.

mod classify;
mod extract;
mod format;
mod index;
mod matching;
mod verdict;

use std::collections::HashMap;

use sha2::{Digest as _, Sha256};

use crate::{CoreError, LineIndex, Sha256Digest};

use extract::{ExtractedRegion, RegionKind};
use index::{CorpusIndex, INDEX_WINDOW_BYTES};
use matching::{
    ConsumptionLog, DestinationMatch, Disposition, EligibleRegion, FragmentResult, run_matching,
};
use verdict::{canonical_reasons, derive_verdict, ledger_identity_holds};

/// Largest number of new-content spans retained in an outcome.
pub const MAX_REPORTED_NEW_SPANS: usize = 50;

/// A borrowed projection of one audited file.
///
/// Paths are normalized relative path BYTES; every path tie-break compares
/// these bytes lexicographically. The digest drives unchanged-vs-modified
/// classification per §7. Phase 2 builds this projection from `srcmv-fs`'s
/// ingested trees.
#[derive(Clone, Copy, Debug)]
pub struct AuditFile<'a> {
    /// Normalized relative path bytes.
    pub path: &'a [u8],
    /// Immutable file bytes.
    pub bytes: &'a [u8],
    /// SHA-256 digest of the immutable bytes.
    pub digest: &'a Sha256Digest,
}

impl AuditFile<'_> {
    /// Returns whether the content looks binary under the reused preview
    /// rule: containing NUL or invalid UTF-8.
    ///
    /// Binary-ness affects presentation only; line ranges report as `None`
    /// for binary files (§9.5).
    #[must_use]
    pub fn is_binary(&self) -> bool {
        self.bytes.contains(&0) || std::str::from_utf8(self.bytes).is_err()
    }
}

/// Matching options validated against the §6.1 resolution constraints.
///
/// Trusted user configuration may tighten floors but can never weaken them:
/// `min_fragment_bytes` must be at least the index window `K = 16` so every
/// matchable span contains a full probe window, and `min_substantial_bytes`
/// must be at least the fragment floor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConservationOptions {
    min_substantial_bytes: u64,
    min_fragment_bytes: u64,
    strict_uniqueness: bool,
    count_formatting: bool,
}

impl ConservationOptions {
    /// Validates and constructs options.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidDomainValue`] when `min_fragment_bytes`
    /// is below the normative index window `K` ([`INDEX_WINDOW_BYTES`], §8.2)
    /// or when `min_substantial_bytes` is below `min_fragment_bytes`, per
    /// §6.1's resolution constraint chain.
    pub const fn new(
        min_substantial_bytes: u64,
        min_fragment_bytes: u64,
        strict_uniqueness: bool,
        count_formatting: bool,
    ) -> Result<Self, CoreError> {
        if min_fragment_bytes < INDEX_WINDOW_BYTES as u64 {
            return Err(CoreError::InvalidDomainValue {
                field: "conservation_min_fragment_bytes",
            });
        }
        if min_substantial_bytes < min_fragment_bytes {
            return Err(CoreError::InvalidDomainValue {
                field: "conservation_min_substantial_bytes",
            });
        }
        Ok(Self {
            min_substantial_bytes,
            min_fragment_bytes,
            strict_uniqueness,
            count_formatting,
        })
    }

    /// Returns the smallest residue worth failing over.
    #[must_use]
    pub const fn min_substantial_bytes(&self) -> u64 {
        self.min_substantial_bytes
    }

    /// Returns the smallest acceptable matched fragment.
    #[must_use]
    pub const fn min_fragment_bytes(&self) -> u64 {
        self.min_fragment_bytes
    }

    /// Returns whether multiplicity-counted matches downgrade to inconclusive
    /// contributors.
    #[must_use]
    pub const fn strict_uniqueness(&self) -> bool {
        self.strict_uniqueness
    }

    /// Returns whether formatting-only deltas count as failures.
    #[must_use]
    pub const fn count_formatting(&self) -> bool {
        self.count_formatting
    }
}

impl Default for ConservationOptions {
    fn default() -> Self {
        Self {
            min_substantial_bytes: 32,
            min_fragment_bytes: INDEX_WINDOW_BYTES as u64,
            strict_uniqueness: false,
            count_formatting: false,
        }
    }
}

/// Deterministic work and memory budgets charged by the engine.
///
/// Fields stay private behind trusted constructors: release values come from
/// [`Default`], tests may pass arbitrary values including `u64::MAX`, and
/// checked arithmetic precedes every allocation the engine makes. The index
/// window width is deliberately absent: §8.2 fixes it normatively as
/// [`INDEX_WINDOW_BYTES`], so only `hot_bucket_limit` remains tunable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConservationBudget {
    extraction_work_units: u64,
    anchor_effort_cap_per_pair: u64,
    matching_work_units: u64,
    index_memory_bytes: u64,
    retained_regions: u64,
    hot_bucket_limit: u64,
}

impl ConservationBudget {
    /// Constructs a budget from explicit values; trusted callers only.
    #[must_use]
    pub const fn new(
        extraction_work_units: u64,
        anchor_effort_cap_per_pair: u64,
        matching_work_units: u64,
        index_memory_bytes: u64,
        retained_regions: u64,
        hot_bucket_limit: u64,
    ) -> Self {
        Self {
            extraction_work_units,
            anchor_effort_cap_per_pair,
            matching_work_units,
            index_memory_bytes,
            retained_regions,
            hot_bucket_limit,
        }
    }

    /// Returns the shared extraction pool size in work units.
    #[must_use]
    pub const fn extraction_work_units(&self) -> u64 {
        self.extraction_work_units
    }

    /// Returns the per-file-pair anchor-pass soft cap in extraction units.
    #[must_use]
    pub const fn anchor_effort_cap_per_pair(&self) -> u64 {
        self.anchor_effort_cap_per_pair
    }

    /// Returns the shared matching pool size in work units.
    #[must_use]
    pub const fn matching_work_units(&self) -> u64 {
        self.matching_work_units
    }

    /// Returns the corpus index memory cap in bytes.
    #[must_use]
    pub const fn index_memory_bytes(&self) -> u64 {
        self.index_memory_bytes
    }

    /// Returns the maximum number of removed regions retained.
    #[must_use]
    pub const fn retained_regions(&self) -> u64 {
        self.retained_regions
    }

    /// Returns the hot-bucket suppression threshold in positions per key.
    #[must_use]
    pub const fn hot_bucket_limit(&self) -> u64 {
        self.hot_bucket_limit
    }
}

impl Default for ConservationBudget {
    fn default() -> Self {
        Self {
            extraction_work_units: 200_000_000,
            anchor_effort_cap_per_pair: 50_000_000,
            matching_work_units: 400_000_000,
            index_memory_bytes: 2 * 1024 * 1024 * 1024,
            retained_regions: 200_000,
            hot_bucket_limit: 64,
        }
    }
}

/// Borrowed audit inputs: two tree projections plus options and budget.
#[derive(Clone, Copy, Debug)]
pub struct ConservationInput<'a> {
    /// Before tree, ideally sorted by normalized path bytes; the engine
    /// sorts defensively either way.
    pub base: &'a [AuditFile<'a>],
    /// After tree, ideally sorted by normalized path bytes.
    pub head: &'a [AuditFile<'a>],
    /// Validated matching options.
    pub options: ConservationOptions,
    /// Work and memory budgets.
    pub budget: ConservationBudget,
}

/// Final verdict of one completed audit run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Verdict {
    /// Every substantial removed byte was byte-verified against exclusively
    /// consumed intervals, with both carve-outs reported separately.
    Pass,
    /// At least one substantial removed region has no remaining exact
    /// occurrence available under exclusive consumption.
    Fail,
    /// The analysis could not complete reliably; consult the reason list.
    Inconclusive,
}

/// Machine-readable fatal conditions forcing `inconclusive`, spelled exactly
/// as their §9.3 reason strings through [`InconclusiveReason::as_str`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InconclusiveReason {
    /// Extraction or matching ran out of work units; processing stopped
    /// deterministically and remaining bytes landed in `unanalyzed`.
    WorkBudgetExceeded,
    /// Index memory failed after matching had already begun. Reserved for the
    /// §9.3 downgrade boundary; structurally unreachable under the v1 fixed
    /// pipeline because the index completes before the first probe.
    MemoryBudgetExceeded,
    /// One or more files were skipped during ingestion upstream.
    UnreadableInputSkipped,
    /// Multiplicity-counted matches exist under strict-uniqueness mode.
    StrictUniquenessAmbiguity,
    /// At least one region has no cold index window anywhere in the head
    /// tree after hot-bucket suppression.
    AmbiguousCommon,
    /// Serialized-report overflow dropped listings upstream.
    ReportTruncated,
}

impl InconclusiveReason {
    /// Returns the exact §9.3 reason string.
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

/// Semantic values behind the thirteen §11.1 totals keys.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Totals {
    /// Number of removal regions enumerated, including placeholders.
    pub removed_regions: u64,
    /// Sum of all region lengths.
    pub removed_bytes: u64,
    /// Exact plus fragment-conserved bytes.
    pub conserved_bytes: u64,
    /// Bytes matched by whole-region verification.
    pub conserved_exact_bytes: u64,
    /// Bytes matched fragmentarily.
    pub conserved_fragments_bytes: u64,
    /// Full length of line-edge-equivalent rewrites.
    pub formatting_only_bytes: u64,
    /// Residue below the substantiality floor.
    pub insubstantial_residue_bytes: u64,
    /// Residue at or above the substantiality floor.
    pub unmatched_substantial_bytes: u64,
    /// Full length of regions without any cold window.
    pub ambiguous_common_bytes: u64,
    /// Bytes the budgets never reached.
    pub unanalyzed_bytes: u64,
    /// Insertion-span survivors after subtracting consumed overlaps.
    pub new_bytes: u64,
    /// Regions whose pairing relied on multiplicity counting.
    pub pairing_ambiguous_regions: u64,
    /// Regions classified ambiguous-common.
    pub ambiguous_common_regions: u64,
}

/// Closed record status vocabulary for emitted regions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionStatus {
    /// Fully accounted by verified destinations.
    Conserved,
    /// Carries substantial unmatched residue.
    UnmatchedSubstantial,
    /// Line-edge-equivalent rewrite of its paired context.
    FormattingOnly,
}

/// One head-side destination span of a conserved or partially matched region.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationRecord {
    /// Head-side path bytes.
    pub path: Vec<u8>,
    /// Half-open byte range inside the head file.
    pub byte_range: (u64, u64),
    /// One-based inclusive line range, or `None` for binary files.
    pub line_range: Option<(u64, u64)>,
    /// SHA-256 of the destination bytes.
    pub sha256: Sha256Digest,
}

/// Paired insertion context attached to formatting-only records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairedDestination {
    /// Head-side path bytes of the counterpart.
    pub path: Vec<u8>,
    /// Half-open byte range of the counterpart.
    pub byte_range: (u64, u64),
}

/// One reviewer-facing removal record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionRecord {
    /// Stable identifier assigned over ALL extracted regions in
    /// `(source path, start, end)` order. Emitted ids may be sparse because
    /// regions without reviewer status — insubstantial-residue,
    /// ambiguous-common, and unanalyzed placeholders — produce no record.
    pub id: u64,
    /// Base-side path bytes.
    pub source_path: Vec<u8>,
    /// Half-open byte range inside the base file.
    pub source_byte_range: (u64, u64),
    /// One-based inclusive line range, or `None` for binary files.
    pub source_line_range: Option<(u64, u64)>,
    /// Removed length in bytes.
    pub length: u64,
    /// SHA-256 of the exact removed bytes.
    pub sha256: Sha256Digest,
    /// Record classification.
    pub status: RegionStatus,
    /// Whether copy pairing relied on multiplicity counting.
    pub pairing_ambiguous: bool,
    /// Destinations ordered by `(path, offset)`; fragments appear as separate
    /// entries, making splits visible.
    pub destinations: Vec<DestinationRecord>,
    /// Formatting records carry their paired insertion context.
    pub paired_context: Option<PairedDestination>,
}

/// One largest surviving insertion span for §8.8 reporting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewContentSpan {
    /// Head-side path bytes.
    pub path: Vec<u8>,
    /// Half-open insertion-span range before consumption clipping.
    pub byte_range: (u64, u64),
    /// Surviving length after subtracting consumed overlaps.
    pub length: u64,
    /// SHA-256 of the full insertion-span bytes.
    pub sha256: Sha256Digest,
}

/// Complete deterministic outcome of one audit run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConservationOutcome {
    /// Derived verdict per §9.1's strict order.
    pub verdict: Verdict,
    /// Deduplicated canonical-order inconclusive reasons.
    pub reasons: Vec<InconclusiveReason>,
    /// Ledger totals recomputed from per-region outcomes.
    pub totals: Totals,
    /// Records for conserved, unmatched-substantial, and formatting-only
    /// classifications; insubstantial-residue and ambiguous-common bytes
    /// appear through totals and counts only.
    pub records: Vec<RegionRecord>,
    /// Largest new spans, ordered by surviving length then `(path, offset)`.
    pub new_content: Vec<NewContentSpan>,
}

/// Audits conservation between a base tree and a head tree.
///
/// The function is pure and deterministic: no filesystem access, no clocks,
/// no randomness, borrowed slices only.
///
/// # Errors
///
/// Returns [`CoreError::ResourceLimitExceeded`] when the corpus index cannot
/// be built within the index-memory budget, or when more than
/// `budget.retained_regions()` must be retained. Both failures occur before
/// any region is matched, mapping to `RESOURCE_LIMIT_EXCEEDED` per §9.3's
/// mechanical boundary. Index-memory failures *after* matching began would
/// downgrade to [`InconclusiveReason::MemoryBudgetExceeded`] with partial
/// results preserved; that branch is structurally unreachable in v1 because
/// the index builds exactly once before the first probe, and this doc comment
/// pins the chosen boundary model.
pub fn audit_conservation(input: ConservationInput<'_>) -> Result<ConservationOutcome, CoreError> {
    let base = sort_by_path(input.base);
    let head = sort_by_path(input.head);
    debug_assert!(
        paths_unique(&base) && paths_unique(&head),
        "caller contract: normalized paths are unique within each tree"
    );

    let diff = classify::classify(&base, &head);
    let created_heads = diff.created;

    // Extraction pool with the §10 stop-and-unanalyzed rule.
    let mut extraction_pool = WorkPool::new(input.budget.extraction_work_units());
    let extraction = extract::extract_all(
        &base,
        &head,
        &diff.modified,
        &diff.deleted,
        &input.budget,
        &mut extraction_pool,
    )?;

    let mut removed_multiplicity: HashMap<[u8; 32], u64> = HashMap::new();
    for region in &extraction.regions {
        *removed_multiplicity.entry(region.digest.0).or_default() += 1;
    }

    // Eligibility: formatting-classified, placeholder, and sub-K regions
    // never probe; everything else enters the deterministic priority order.
    let mut eligible_slots: Vec<usize> = extraction
        .regions
        .iter()
        .enumerate()
        .filter(|(_, region)| {
            !region.formatting_only
                && !matches!(region.kind, RegionKind::UnanalyzedPlaceholder)
                && region.length() >= INDEX_WINDOW_BYTES as u64
        })
        .map(|(index, _)| index)
        .collect();
    eligible_slots.sort_by(|&left, &right| {
        let longer = &extraction.regions[left];
        let shorter = &extraction.regions[right];
        shorter
            .length()
            .cmp(&longer.length())
            .then(
                base[longer.base_file]
                    .path
                    .cmp(base[shorter.base_file].path),
            )
            .then(longer.start.cmp(&shorter.start))
    });

    // Index build happens once, entirely before matching begins, so its
    // memory failures always surface as the early envelope error (§9.3).
    let corpus_index = CorpusIndex::build(&head, &input.budget)?;

    let mut matching_pool = WorkPool::new(input.budget.matching_work_units());
    let eligible: Vec<EligibleRegion<'_>> = eligible_slots
        .iter()
        .map(|&region_index| {
            let region = &extraction.regions[region_index];
            EligibleRegion {
                digest: region.digest,
                bytes: &base[region.base_file].bytes[region.start as usize..region.end as usize],
            }
        })
        .collect();
    let outputs = run_matching(
        &eligible,
        &head,
        &corpus_index,
        &input.options,
        &mut matching_pool,
        &removed_multiplicity,
    );

    let mut rows: Vec<LedgerRow> = extraction
        .regions
        .iter()
        .map(|region| LedgerRow {
            region: *region,
            destinations: Vec::new(),
            exact_bytes: 0,
            fragment_bytes: 0,
            insubstantial_residue_bytes: 0,
            unmatched_substantial_bytes: 0,
            formatting_only_bytes: 0,
            ambiguous_common_bytes: 0,
            unanalyzed_bytes: 0,
            pairing_ambiguous: false,
        })
        .collect();

    let mut work_budget_exceeded = extraction.pool_exhausted;
    for slot in 0..eligible.len() {
        let row = &mut rows[eligible_slots[slot]];
        let region_len = row.region.length();
        match &outputs.dispositions[slot] {
            Disposition::Exact {
                destination,
                pairing_ambiguous,
            } => {
                row.exact_bytes = region_len;
                row.destinations.push(*destination);
                row.pairing_ambiguous = *pairing_ambiguous;
            }
            Disposition::AmbiguousCommon => {
                row.ambiguous_common_bytes = region_len;
            }
            Disposition::Fragments(FragmentResult {
                destinations,
                conserved,
                residue,
                unanalyzed,
                pairing_ambiguous,
            }) => {
                row.fragment_bytes = *conserved;
                row.unanalyzed_bytes = *unanalyzed;
                work_budget_exceeded |= *unanalyzed > 0;
                if *residue >= input.options.min_substantial_bytes() {
                    row.unmatched_substantial_bytes = *residue;
                } else {
                    row.insubstantial_residue_bytes = *residue;
                }
                row.destinations.extend_from_slice(destinations);
                row.pairing_ambiguous = *pairing_ambiguous;
            }
            Disposition::NotProcessed => {
                row.unanalyzed_bytes = region_len;
                work_budget_exceeded = true;
            }
        }
    }

    // Formatting gate results and sub-K regions settle here; both never
    // reach matching by construction.
    for row in &mut rows {
        let region_len = row.region.length();
        if matches!(row.region.kind, RegionKind::UnanalyzedPlaceholder) {
            row.unanalyzed_bytes = region_len;
        } else if row.region.formatting_only {
            row.formatting_only_bytes = region_len;
        } else if region_len < INDEX_WINDOW_BYTES as u64 && row.is_untouched() {
            row.insubstantial_residue_bytes = region_len;
        }
        debug_assert_eq!(
            row.ledger_sum(),
            region_len,
            "per-region ledger terms must partition the region length"
        );
    }

    let mut reasons: Vec<InconclusiveReason> = Vec::new();
    if work_budget_exceeded {
        reasons.push(InconclusiveReason::WorkBudgetExceeded);
    }
    if rows.iter().any(|row| row.ambiguous_common_bytes > 0) {
        reasons.push(InconclusiveReason::AmbiguousCommon);
    }
    if input.options.strict_uniqueness() && rows.iter().any(|row| row.pairing_ambiguous) {
        reasons.push(InconclusiveReason::StrictUniquenessAmbiguity);
    }

    let presentation_order = ordered_region_ids(&rows, &base);
    let mut lines = LineRangeBuilder::default();
    let mut records = Vec::new();
    for (id, &region_index) in presentation_order.iter().enumerate() {
        let row = &rows[region_index];
        let Some(status) = row.record_status() else {
            continue;
        };
        let source_file = &base[row.region.base_file];
        records.push(RegionRecord {
            id: id as u64,
            source_path: source_file.path.to_vec(),
            source_byte_range: (row.region.start, row.region.end),
            source_line_range: lines.line_range(source_file, row.region.start, row.region.end),
            length: row.region.length(),
            sha256: row.region.digest,
            status,
            pairing_ambiguous: row.pairing_ambiguous,
            destinations: destination_records(row.destinations.clone(), &head, &mut lines),
            paired_context: row.paired_context().map(|context| PairedDestination {
                path: head[context.head_file].path.to_vec(),
                byte_range: (context.start, context.end),
            }),
        });
    }

    let new_content = collect_new_content(
        &extraction.insertion_spans,
        &created_heads,
        &head,
        &outputs.consumption,
    );
    let new_bytes = new_content.iter().map(|span| span.length).sum();

    let totals = assemble_totals(&rows, new_bytes);
    debug_assert!(
        ledger_identity_holds(&totals),
        "§9.2 identity violated: {totals:?}"
    );
    debug_assert!(
        report_consistency_holds(&outputs.consumption, &new_content, &head),
        "consumed intervals and new spans must stay disjoint and within bounds"
    );
    if !ledger_identity_holds(&totals) {
        return Err(CoreError::InvalidDomainValue {
            field: "conservation_ledger_identity",
        });
    }
    // Release-time §8.8/§9.2 report-level recomputation: consumed intervals
    // pairwise disjoint and bounded per head file, new spans in bounds, and
    // the surviving length of every span exactly its length minus the
    // consumed overlap, so `consumed ∪ new_spans` never overlaps.
    if !report_consistency_holds(&outputs.consumption, &new_content, &head) {
        return Err(CoreError::InvalidDomainValue {
            field: "conservation_report_consistency",
        });
    }

    let reasons = canonical_reasons(reasons);
    let verdict = derive_verdict(&reasons, &totals, input.options.count_formatting());

    Ok(ConservationOutcome {
        verdict,
        reasons,
        totals,
        records,
        new_content,
    })
}

/// Shared mutable extraction/matching unit pool.
pub(crate) struct WorkPool {
    remaining: u64,
}

impl WorkPool {
    pub(crate) const fn new(units: u64) -> Self {
        Self { remaining: units }
    }

    /// Deducts units; returns `false` exactly when the pool cannot cover the
    /// request, zeroing the remainder so exhaustion latches deterministically.
    pub(crate) fn spend(&mut self, units: u64) -> bool {
        if units <= self.remaining {
            self.remaining -= units;
            true
        } else {
            self.remaining = 0;
            false
        }
    }

    pub(crate) const fn exhausted(&self) -> bool {
        self.remaining == 0
    }
}

/// SHA-256 helper shared by extraction, matching, and new-content assembly.
pub(crate) fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(Sha256::digest(bytes).into())
}

/// Per-region ledger bookkeeping; its terms always partition the length.
struct LedgerRow {
    region: ExtractedRegion,
    destinations: Vec<DestinationMatch>,
    exact_bytes: u64,
    fragment_bytes: u64,
    insubstantial_residue_bytes: u64,
    unmatched_substantial_bytes: u64,
    formatting_only_bytes: u64,
    ambiguous_common_bytes: u64,
    unanalyzed_bytes: u64,
    pairing_ambiguous: bool,
}

impl LedgerRow {
    const fn is_untouched(&self) -> bool {
        self.exact_bytes
            + self.fragment_bytes
            + self.insubstantial_residue_bytes
            + self.unmatched_substantial_bytes
            + self.formatting_only_bytes
            + self.ambiguous_common_bytes
            + self.unanalyzed_bytes
            == 0
    }

    const fn ledger_sum(&self) -> u64 {
        self.exact_bytes
            + self.fragment_bytes
            + self.insubstantial_residue_bytes
            + self.unmatched_substantial_bytes
            + self.formatting_only_bytes
            + self.ambiguous_common_bytes
            + self.unanalyzed_bytes
    }

    fn record_status(&self) -> Option<RegionStatus> {
        if self.formatting_only_bytes > 0 {
            return Some(RegionStatus::FormattingOnly);
        }
        if self.unmatched_substantial_bytes > 0 {
            return Some(RegionStatus::UnmatchedSubstantial);
        }
        if self.exact_bytes + self.fragment_bytes > 0 {
            return Some(RegionStatus::Conserved);
        }
        None
    }

    const fn paired_context(&self) -> Option<extract::PairedContext> {
        match self.region.kind {
            RegionKind::GapDerived { context } => context,
            RegionKind::WholeDeletedFile | RegionKind::UnanalyzedPlaceholder => None,
        }
    }
}

fn sort_by_path<'a>(files: &'a [AuditFile<'a>]) -> Vec<AuditFile<'a>> {
    let mut sorted: Vec<AuditFile<'_>> = files.to_vec();
    sorted.sort_by(|left, right| left.path.cmp(right.path));
    sorted
}

fn paths_unique(files: &[AuditFile<'_>]) -> bool {
    files.windows(2).all(|pair| pair[0].path != pair[1].path)
}

/// Assigns ids over all regions in `(source path, start, end)` order so ids
/// stay stable regardless of classification outcomes.
fn ordered_region_ids(rows: &[LedgerRow], base: &[AuditFile<'_>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&left, &right| {
        let left_row = &rows[left];
        let right_row = &rows[right];
        base[left_row.region.base_file]
            .path
            .cmp(base[right_row.region.base_file].path)
            .then(left_row.region.start.cmp(&right_row.region.start))
            .then(left_row.region.end.cmp(&right_row.region.end))
    });
    order
}

fn destination_records(
    destinations: Vec<DestinationMatch>,
    head: &[AuditFile<'_>],
    lines: &mut LineRangeBuilder,
) -> Vec<DestinationRecord> {
    let mut ordered = destinations;
    ordered.sort_by(|left, right| {
        head[left.head_file]
            .path
            .cmp(head[right.head_file].path)
            .then(left.start.cmp(&right.start))
    });
    ordered
        .into_iter()
        .map(|destination| {
            let file = &head[destination.head_file];
            DestinationRecord {
                path: file.path.to_vec(),
                byte_range: (destination.start, destination.end),
                line_range: lines.line_range(file, destination.start, destination.end),
                sha256: destination.digest,
            }
        })
        .collect()
}

/// Lazily builds line indexes for reported ranges, skipping binary files.
///
/// Indexes build under the house per-call caps, mirroring the snapshot
/// vocabulary of `srcmv-fs`'s `SnapshotLimits` (plan §5, §7): at most
/// [`REPORT_LINE_INDEX_MAX_LINES`] lines and
/// [`REPORT_LINE_INDEX_MEMORY_BYTES`] of line-index memory per construction.
/// The values are hard-coded here because `srcmv-core` cannot import the
/// filesystem crate; Phase 2's ingestion shares the same vocabulary from the
/// `srcmv-fs` side.
#[derive(Default)]
struct LineRangeBuilder {
    cache: HashMap<usize, Option<LineIndex>>,
}

/// House per-call line cap for report assembly (`SnapshotLimits`, §5/§7).
const REPORT_LINE_INDEX_MAX_LINES: u64 = 5_000_000;

/// House per-call line-index memory cap for report assembly in bytes
/// (256 MiB per `SnapshotLimits`, §5/§7).
const REPORT_LINE_INDEX_MEMORY_BYTES: u64 = 256 * 1024 * 1024;

impl LineRangeBuilder {
    fn line_range(&mut self, file: &AuditFile<'_>, start: u64, end: u64) -> Option<(u64, u64)> {
        let key = file.bytes.as_ptr() as usize;
        let index = self
            .cache
            .entry(key)
            .or_insert_with(|| {
                if file.is_binary() || file.bytes.is_empty() {
                    None
                } else {
                    LineIndex::from_bytes_with_limits(
                        file.bytes,
                        REPORT_LINE_INDEX_MAX_LINES,
                        REPORT_LINE_INDEX_MEMORY_BYTES,
                    )
                    .ok()
                }
            })
            .as_ref()?;
        let count = index.line_count();
        if end <= start || count == 0 {
            return None;
        }
        let first = last_line_with_start_at_or_before(index, count, start);
        let last = last_line_with_start_before(index, count, end);
        (last > 0).then_some((first, last))
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

fn collect_new_content(
    insertion_spans: &[(usize, u64, u64)],
    created_heads: &[usize],
    head: &[AuditFile<'_>],
    consumption: &[ConsumptionLog],
) -> Vec<NewContentSpan> {
    let mut spans: Vec<(usize, u64, u64)> = insertion_spans.to_vec();
    for &created in created_heads {
        let length = head[created].bytes.len() as u64;
        if length > 0 {
            spans.push((created, 0, length));
        }
    }
    let mut survivors: Vec<NewContentSpan> = spans
        .into_iter()
        .filter_map(|(file, start, end)| {
            let span_len = end - start;
            let consumed_inside = overlap_with(consumption[file].intervals(), start, end);
            let surviving = span_len.saturating_sub(consumed_inside);
            debug_assert_eq!(
                consumed_inside,
                span_len - surviving,
                "consumed clipping must account for the full difference"
            );
            (surviving > 0).then(|| NewContentSpan {
                path: head[file].path.to_vec(),
                byte_range: (start, end),
                length: surviving,
                sha256: sha256_digest(&head[file].bytes[start as usize..end as usize]),
            })
        })
        .collect();
    survivors.sort_by(|left, right| {
        right
            .length
            .cmp(&left.length)
            .then(left.path.cmp(&right.path))
            .then(left.byte_range.cmp(&right.byte_range))
    });
    survivors.truncate(MAX_REPORTED_NEW_SPANS);
    survivors
}

fn overlap_with(intervals: &[(u64, u64)], start: u64, end: u64) -> u64 {
    let first_past = intervals.partition_point(|&(low, _)| low < end);
    let mut overlap = 0_u64;
    for &(low, high) in intervals[..first_past].iter().rev() {
        if high <= start {
            break;
        }
        let clipped_low = low.max(start);
        let clipped_high = high.min(end);
        overlap += clipped_high.saturating_sub(clipped_low);
    }
    overlap
}

fn assemble_totals(rows: &[LedgerRow], new_bytes: u64) -> Totals {
    let mut totals = Totals {
        removed_regions: rows.len() as u64,
        new_bytes,
        ..Totals::default()
    };
    for row in rows {
        totals.removed_bytes += row.region.length();
        totals.conserved_exact_bytes += row.exact_bytes;
        totals.conserved_fragments_bytes += row.fragment_bytes;
        totals.insubstantial_residue_bytes += row.insubstantial_residue_bytes;
        totals.unmatched_substantial_bytes += row.unmatched_substantial_bytes;
        totals.formatting_only_bytes += row.formatting_only_bytes;
        totals.ambiguous_common_bytes += row.ambiguous_common_bytes;
        totals.unanalyzed_bytes += row.unanalyzed_bytes;
    }
    totals.conserved_bytes = totals.conserved_exact_bytes + totals.conserved_fragments_bytes;
    totals.pairing_ambiguous_regions =
        rows.iter().filter(|row| row.pairing_ambiguous).count() as u64;
    totals.ambiguous_common_regions = rows
        .iter()
        .filter(|row| row.ambiguous_common_bytes > 0)
        .count() as u64;
    totals
}

/// Release-time §8.8/§9.2 report-level consistency recomputation.
///
/// Per head file the consumed intervals must be pairwise disjoint and within
/// file bounds; every new-content span must lie within its hosting file; and
/// each span's surviving length must equal its span length minus the consumed
/// overlap inside it, so `consumed ∪ new_spans` never overlaps and their
/// union stays within file bounds.
fn report_consistency_holds(
    consumption: &[ConsumptionLog],
    new_content: &[NewContentSpan],
    head: &[AuditFile<'_>],
) -> bool {
    let disjoint_and_bounded = consumption.iter().zip(head).all(|(log, file)| {
        log.intervals().windows(2).all(|pair| pair[0].1 < pair[1].0)
            && log
                .intervals()
                .iter()
                .all(|&(low, high)| low < high && high <= file.bytes.len() as u64)
    });
    if !disjoint_and_bounded {
        return false;
    }
    let rank_of: HashMap<&[u8], usize> = head
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path, index))
        .collect();
    new_content.iter().all(|span| {
        let Some(&file) = rank_of.get(span.path.as_slice()) else {
            return false;
        };
        let (start, end) = span.byte_range;
        let in_bounds = start < end
            && end <= head[file].bytes.len() as u64
            && span.length > 0
            && span.length <= end - start;
        let consumed_inside = overlap_with(consumption[file].intervals(), start, end);
        in_bounds && consumed_inside == end - start - span.length
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{AuditFile, sha256_digest};
    use crate::{ConservationBudget, Sha256Digest};

    /// Builds an [`AuditFile`] from explicit borrowed parts.
    pub(crate) fn audit_file<'a>(
        path: &'a [u8],
        bytes: &'a [u8],
        digest: &'a Sha256Digest,
    ) -> AuditFile<'a> {
        AuditFile {
            path,
            bytes,
            digest,
        }
    }

    /// Leaks a deterministic digest so digest-tie scenarios can share it.
    pub(crate) fn leaked_digest(seed: u8) -> &'static Sha256Digest {
        Box::leak(Box::new(Sha256Digest([seed; 32])))
    }

    /// Builds a fully owned leaked file with a real content digest.
    pub(crate) fn leak_file(path: &[u8], bytes: Vec<u8>) -> AuditFile<'static> {
        let digest = sha256_digest(&bytes);
        AuditFile {
            path: Box::leak(path.to_vec().into_boxed_slice()),
            bytes: Box::leak(bytes.into_boxed_slice()),
            digest: Box::leak(Box::new(digest)),
        }
    }

    pub(crate) fn unlimited_budget() -> ConservationBudget {
        ConservationBudget::default()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn budget_with(
        extraction_work_units: u64,
        anchor_effort_cap_per_pair: u64,
        matching_work_units: u64,
        index_memory_bytes: u64,
        retained_regions: u64,
        hot_bucket_limit: u64,
    ) -> ConservationBudget {
        ConservationBudget::new(
            extraction_work_units,
            anchor_effort_cap_per_pair,
            matching_work_units,
            index_memory_bytes,
            retained_regions,
            hot_bucket_limit,
        )
    }

    /// Owned two-tree fixture builder for engine-level scenarios.
    pub(crate) struct AuditedTree {
        paths: Vec<Vec<u8>>,
        contents: Vec<Vec<u8>>,
        digests: Vec<Sha256Digest>,
    }

    impl AuditedTree {
        pub(crate) fn new(files: &[(&[u8], &[u8])]) -> Self {
            Self {
                paths: files.iter().map(|(path, _)| path.to_vec()).collect(),
                digests: files
                    .iter()
                    .map(|(_, bytes)| sha256_digest(bytes))
                    .collect(),
                contents: files.iter().map(|(_, bytes)| bytes.to_vec()).collect(),
            }
        }

        pub(crate) fn files(&self) -> Vec<AuditFile<'_>> {
            self.paths
                .iter()
                .zip(self.contents.iter().zip(&self.digests))
                .map(|(path, (bytes, digest))| AuditFile {
                    path,
                    bytes,
                    digest,
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::matching::ConsumptionLog;
    use super::{
        ConservationInput, ConservationOptions, ConservationOutcome, InconclusiveReason,
        NewContentSpan, RegionStatus, Verdict, audit_conservation, report_consistency_holds,
        sha256_digest, test_support::*,
    };
    use crate::CoreError;

    fn default_options() -> ConservationOptions {
        ConservationOptions::default()
    }

    fn strict_options() -> ConservationOptions {
        ConservationOptions::new(32, 16, true, false).expect("strict options should validate")
    }

    fn count_formatting_options() -> ConservationOptions {
        ConservationOptions::new(32, 16, false, true).expect("counting options should validate")
    }

    fn audit(base: &AuditedTree, head: &AuditedTree) -> ConservationOutcome {
        audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: default_options(),
            budget: unlimited_budget(),
        })
        .expect("unlimited-budget audits should not error")
    }

    fn assert_ledger_identity(outcome: &ConservationOutcome) {
        let totals = outcome.totals;
        let sum = totals.conserved_exact_bytes
            + totals.conserved_fragments_bytes
            + totals.unmatched_substantial_bytes
            + totals.insubstantial_residue_bytes
            + totals.formatting_only_bytes
            + totals.ambiguous_common_bytes
            + totals.unanalyzed_bytes;
        assert_eq!(sum, totals.removed_bytes, "§9.2 identity");
    }

    const MOVE_PAYLOAD: &[u8] = b"move-across-files-payload-Alpha-0123456789012345";

    /// Identical removed content for the §8.6 duplicate matrix; comfortably
    /// above both the index window and the substantiality floor.
    const MATRIX_CONTENT: &[u8] = b"duplicate-matrix-payload-theta-01234567890123456789012345";

    #[test]
    fn audit_should_pass_move_across_files_per_fixture_recipe() {
        // Recipe from tests/fixtures/conservation/pass-move-across-files:
        // deleted payload reappears inside a created wrapper file.
        let mut wrapped = b"// applied:\n".to_vec();
        wrapped.extend_from_slice(MOVE_PAYLOAD);
        wrapped.extend_from_slice(b"\n// end\n");

        let base = AuditedTree::new(&[
            (b"docs/note.txt", b"unchanged bystander\n"),
            (b"src/feature.txt", MOVE_PAYLOAD),
        ]);
        let head = AuditedTree::new(&[
            (b"docs/note.txt", b"unchanged bystander\n"),
            (b"dst/feature-applied.txt", &wrapped),
        ]);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert!(outcome.reasons.is_empty());
        assert_eq!(outcome.totals.removed_regions, 1);
        assert_eq!(
            outcome.totals.conserved_exact_bytes,
            MOVE_PAYLOAD.len() as u64
        );
        assert_eq!(
            outcome.totals.new_bytes,
            (wrapped.len() - MOVE_PAYLOAD.len()) as u64
        );
        assert_eq!(outcome.records.len(), 1);
        assert_eq!(outcome.records[0].status, RegionStatus::Conserved);
        assert_eq!(outcome.records[0].destinations.len(), 1);
        assert_eq!(
            outcome.records[0].destinations[0].byte_range,
            (12, 12 + MOVE_PAYLOAD.len() as u64)
        );
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn audit_should_pass_rename_as_delete_create() {
        // Recipe from pass-rename-as-delete-create: byte-identical create at
        // a new path.
        let content: &[u8] = b"rename-scenario-content-Omega-01234567890123456789012345\n";

        let base = AuditedTree::new(&[(b"legacy/old-module.txt", content)]);
        let head = AuditedTree::new(&[(b"modern/new-module.txt", content)]);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.totals.removed_regions, 1);
        assert_eq!(outcome.totals.conserved_exact_bytes, content.len() as u64);
        assert_eq!(outcome.totals.new_bytes, 0);
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn audit_should_pass_reorder_and_consume_the_swapped_block() {
        // Recipe from pass-reorder: blocks A and B swap inside one file. The
        // LIS tie-break anchors B at the smallest after-position; the gap
        // before it trims to exactly A ++ "\n".
        let a: &[u8] = b"reorder-block-first-Beta-012345678901234";
        let b: &[u8] = b"REORDER-BLOCK-SECOND-Gamma-01234567890123456";
        let base_bytes = join(&[
            b"header-line\n".as_slice(),
            a,
            b"\n".as_slice(),
            b,
            b"\ntrailer-line\n".as_slice(),
        ]);
        let head_bytes = join(&[
            b"header-line\n".as_slice(),
            b,
            b"\n".as_slice(),
            a,
            b"\ntrailer-line\n".as_slice(),
        ]);
        let base = AuditedTree::new(&[(b"reorder/data.txt", &base_bytes)]);
        let head = AuditedTree::new(&[(b"reorder/data.txt", &head_bytes)]);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.totals.removed_regions, 1);
        assert_eq!(outcome.totals.conserved_exact_bytes, a.len() as u64 + 1);
        assert_eq!(outcome.totals.new_bytes, 1);
        assert_eq!(
            outcome.records[0].destinations[0].byte_range,
            (57, 98),
            "consumed span per the frozen fixture trace"
        );
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn audit_should_pass_split_block_via_two_cross_file_fragments() {
        // Recipe from pass-split-block: one 64-byte block reappears as two
        // 32-byte halves in separate created files.
        let x: &[u8] = b"split-block-payload-Epsilon-012345678901234567890123456789012345";
        let holder_base = join(&[b"frame-top\n".as_slice(), x, b"\nframe-bottom\n".as_slice()]);
        let part_a = join(&[&x[..32], b"\n"]);
        let part_b = join(&[&x[32..], b"\n"]);

        let base = AuditedTree::new(&[(b"split/holder.txt", &holder_base)]);
        let head = AuditedTree::new(&[
            (b"split/part-a.txt", &part_a),
            (b"split/part-b.txt", &part_b),
            (b"split/holder.txt", b"frame-top\n\nframe-bottom\n"),
        ]);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.totals.removed_regions, 1);
        assert_eq!(outcome.totals.conserved_exact_bytes, 0);
        assert_eq!(outcome.totals.conserved_fragments_bytes, x.len() as u64);
        assert_eq!(outcome.totals.new_bytes, 2);
        assert_eq!(outcome.records[0].destinations.len(), 2);
        assert_eq!(
            outcome.records[0].destinations[0].path,
            b"split/part-a.txt".to_vec()
        );
        assert_eq!(
            outcome.records[0].destinations[1].path,
            b"split/part-b.txt".to_vec()
        );
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn duplicate_removals_should_pass_with_pairing_flags_and_downgrade_under_strict_uniqueness() {
        // Recipe from pass-pairing-ambiguous: two identical removals against
        // two identical survivors.
        let content: &[u8] = b"duplicate-boilerplate-Zeta-0123456789012345678901234\n";
        let base = AuditedTree::new(&[
            (b"dup/copy-one.txt", content),
            (b"dup/copy-two.txt", content),
        ]);
        let head = AuditedTree::new(&[
            (b"keep/retained-a.txt", content),
            (b"keep/retained-b.txt", content),
        ]);

        let lenient = audit(&base, &head);

        assert_eq!(lenient.verdict, Verdict::Pass);
        assert_eq!(lenient.totals.pairing_ambiguous_regions, 2);
        assert_eq!(
            lenient.totals.conserved_exact_bytes,
            content.len() as u64 * 2
        );
        assert_eq!(lenient.totals.new_bytes, 0);
        assert_ledger_identity(&lenient);

        let strict = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: strict_options(),
            budget: unlimited_budget(),
        })
        .expect("strict audit should complete");
        assert_eq!(strict.verdict, Verdict::Inconclusive);
        assert_eq!(
            strict.reasons,
            vec![InconclusiveReason::StrictUniquenessAmbiguity]
        );
        assert_ledger_identity(&strict);
    }

    /// Two identical removed regions (deleted files) against `survivors`
    /// identical head copies — the §8.6 matrix fixture.
    fn duplicate_matrix(survivors: usize) -> (AuditedTree, AuditedTree) {
        let base = AuditedTree::new(&[
            (b"rm/copy-a.txt", MATRIX_CONTENT),
            (b"rm/copy-b.txt", MATRIX_CONTENT),
        ]);
        let mut head_files: Vec<(&[u8], &[u8])> = Vec::new();
        for index in 0..survivors {
            head_files.push(match index {
                0 => (b"keep/survivor-0.txt", MATRIX_CONTENT),
                1 => (b"keep/survivor-1.txt", MATRIX_CONTENT),
                _ => unreachable!("matrix covers at most two survivors"),
            });
        }
        let head = AuditedTree::new(&head_files);
        (base, head)
    }

    #[test]
    fn duplicate_matrix_should_pass_with_two_flags_lenient_when_both_copies_survive() {
        let (base, head) = duplicate_matrix(2);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert!(outcome.reasons.is_empty());
        assert_eq!(outcome.totals.pairing_ambiguous_regions, 2);
        assert_eq!(
            outcome.totals.conserved_exact_bytes,
            2 * MATRIX_CONTENT.len() as u64
        );
        assert_eq!(outcome.totals.unmatched_substantial_bytes, 0);
        assert_eq!(outcome.records.len(), 2);
        assert!(
            outcome
                .records
                .iter()
                .all(|record| record.pairing_ambiguous)
        );
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn duplicate_matrix_should_fail_with_one_flag_lenient_when_one_copy_survives() {
        let (base, head) = duplicate_matrix(1);

        let outcome = audit(&base, &head);

        // The matched copy's record relied on multiplicity counting; the
        // unmatched record took no credit and carries no flag.
        assert_eq!(outcome.verdict, Verdict::Fail);
        assert!(outcome.reasons.is_empty());
        assert_eq!(outcome.totals.pairing_ambiguous_regions, 1);
        assert_eq!(
            outcome.totals.conserved_exact_bytes,
            MATRIX_CONTENT.len() as u64
        );
        assert_eq!(
            outcome.totals.unmatched_substantial_bytes,
            MATRIX_CONTENT.len() as u64
        );
        assert_eq!(outcome.records.len(), 2);
        let flagged: Vec<_> = outcome
            .records
            .iter()
            .filter(|record| record.pairing_ambiguous)
            .collect();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].status, RegionStatus::Conserved);
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn duplicate_matrix_should_fail_without_flags_lenient_when_no_copy_survives() {
        let (base, head) = duplicate_matrix(0);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Fail);
        assert!(outcome.reasons.is_empty());
        assert_eq!(outcome.totals.pairing_ambiguous_regions, 0);
        assert_eq!(
            outcome.totals.unmatched_substantial_bytes,
            2 * MATRIX_CONTENT.len() as u64
        );
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn duplicate_matrix_should_be_inconclusive_strict_when_both_copies_survive() {
        let (base, head) = duplicate_matrix(2);

        let outcome = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: strict_options(),
            budget: unlimited_budget(),
        })
        .expect("strict audit should complete");

        assert_eq!(outcome.verdict, Verdict::Inconclusive);
        assert_eq!(
            outcome.reasons,
            vec![InconclusiveReason::StrictUniquenessAmbiguity]
        );
        assert_eq!(outcome.totals.pairing_ambiguous_regions, 2);
        assert_eq!(
            outcome.totals.conserved_exact_bytes,
            2 * MATRIX_CONTENT.len() as u64
        );
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn duplicate_matrix_should_be_inconclusive_strict_when_one_copy_survives() {
        let (base, head) = duplicate_matrix(1);

        let outcome = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: strict_options(),
            budget: unlimited_budget(),
        })
        .expect("strict audit should complete");

        // §9.1 ordering: the fatal strict-uniqueness reason precedes the
        // unmatched-substantial branch, so this is Inconclusive, not Fail.
        assert_eq!(outcome.verdict, Verdict::Inconclusive);
        assert_eq!(
            outcome.reasons,
            vec![InconclusiveReason::StrictUniquenessAmbiguity]
        );
        assert_eq!(outcome.totals.pairing_ambiguous_regions, 1);
        assert_eq!(
            outcome.totals.unmatched_substantial_bytes,
            MATRIX_CONTENT.len() as u64
        );
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn duplicate_matrix_should_fail_cleanly_strict_when_no_copy_survives() {
        let (base, head) = duplicate_matrix(0);

        let outcome = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: strict_options(),
            budget: unlimited_budget(),
        })
        .expect("strict audit should complete");

        // No credit was taken anywhere, so no region relies on multiplicity
        // counting, carries the flag, or contributes the strict reason.
        assert_eq!(outcome.verdict, Verdict::Fail);
        assert!(outcome.reasons.is_empty());
        assert_eq!(outcome.totals.pairing_ambiguous_regions, 0);
        assert_eq!(
            outcome.totals.unmatched_substantial_bytes,
            2 * MATRIX_CONTENT.len() as u64
        );
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn formatting_only_deltas_should_pass_by_default_and_fail_when_counted() {
        // Recipe from pass-formatting: pure CRLF->LF conversion of three
        // lines yields three single-CR formatting regions.
        let base = AuditedTree::new(&[(b"eol/poem.txt", b"alpha\r\nbravo\r\ncharlie\r\n")]);
        let head = AuditedTree::new(&[(b"eol/poem.txt", b"alpha\nbravo\ncharlie\n")]);

        let lenient = audit(&base, &head);

        assert_eq!(lenient.verdict, Verdict::Pass);
        assert_eq!(lenient.totals.formatting_only_bytes, 3);
        assert_eq!(lenient.totals.removed_regions, 3);
        assert_eq!(lenient.records.len(), 3);
        assert_eq!(lenient.records[0].status, RegionStatus::FormattingOnly);
        // Formatting-classified regions never reach matching and must never
        // carry the strict-uniqueness flag.
        assert!(
            lenient
                .records
                .iter()
                .all(|record| !record.pairing_ambiguous)
        );
        assert_eq!(lenient.totals.pairing_ambiguous_regions, 0);
        assert_eq!(lenient.totals.ambiguous_common_regions, 0);
        let context = lenient.records[0]
            .paired_context
            .clone()
            .expect("gap-derived");
        assert_eq!(context.path, b"eol/poem.txt".to_vec());
        assert_ledger_identity(&lenient);

        let counted = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: count_formatting_options(),
            budget: unlimited_budget(),
        })
        .expect("counted audit should complete");
        assert_eq!(counted.verdict, Verdict::Fail);
        assert!(counted.reasons.is_empty());
        assert_ledger_identity(&counted);
    }

    #[test]
    fn deleting_a_substantial_block_without_recreation_should_fail() {
        // Recipe from fail-deleted-block.
        let notes: &[u8] =
            b"vanishing-block-alpha\nsecond-line-beta-goes-too\nthird-line-gamma-finishes\n";
        let base = AuditedTree::new(&[
            (b"keeper.txt", b"keeper line stays\n"),
            (b"vanished/unique-notes.md", notes),
        ]);
        let head = AuditedTree::new(&[(b"keeper.txt", b"keeper line stays\n")]);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Fail);
        assert!(outcome.reasons.is_empty());
        assert_eq!(
            outcome.totals.unmatched_substantial_bytes,
            notes.len() as u64
        );
        assert_eq!(outcome.records.len(), 1);
        let record = &outcome.records[0];
        assert_eq!(record.status, RegionStatus::UnmatchedSubstantial);
        assert_eq!(record.source_path, b"vanished/unique-notes.md".to_vec());
        assert_eq!(record.length, notes.len() as u64);
        assert!(record.destinations.is_empty());
        assert_eq!(record.source_line_range, Some((1, 3)));
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn whitespace_heavy_deletions_without_cold_windows_should_be_ambiguous_common() {
        // Recipe from inconclusive-ambiguous-common.
        let base = AuditedTree::new(&[(b"ws/heavy-spaces.txt", &[b' '; 200])]);
        let head = AuditedTree::new(&[(b"ws/bulk-spaces.txt", vec![b' '; 4000].as_slice())]);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Inconclusive);
        assert_eq!(outcome.reasons, vec![InconclusiveReason::AmbiguousCommon]);
        assert_eq!(outcome.totals.ambiguous_common_bytes, 200);
        assert_eq!(outcome.totals.ambiguous_common_regions, 1);
        assert_eq!(outcome.totals.removed_regions, 1);
        assert!(outcome.records.is_empty());
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn extraction_pool_exhaustion_should_ledger_everything_unanalyzed() {
        let base = AuditedTree::new(&[
            (b"a.txt", b"changed-payload-gone-forever\n"),
            (b"b.txt", b"deleted-whole-file-bytes\n"),
        ]);
        let head = AuditedTree::new(&[(b"a.txt", b"rewritten entirely\n")]);
        let dry = budget_with(1, u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64);

        let outcome = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: default_options(),
            budget: dry,
        })
        .expect("placeholder synthesis should not error");

        assert_eq!(outcome.verdict, Verdict::Inconclusive);
        assert_eq!(
            outcome.reasons,
            vec![InconclusiveReason::WorkBudgetExceeded]
        );
        assert_eq!(
            outcome.totals.unanalyzed_bytes,
            outcome.totals.removed_bytes
        );
        assert_eq!(outcome.totals.removed_regions, 2);
        assert!(outcome.records.is_empty());
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn matching_pool_exhaustion_should_cut_mid_region_into_unanalyzed() {
        // One large unique block plus a tiny matching pool: the exact scan
        // alone cannot finish, so the remainder lands in unanalyzed.
        let block = vec![b'k'; 512];
        let base = AuditedTree::new(&[(b"big.txt", block.as_slice())]);
        let head = AuditedTree::new(&[(b"other.txt", b"nothing-matches-here\n")]);
        let tiny_matching = budget_with(u64::MAX, u64::MAX, 8, u64::MAX, u64::MAX, 64);

        let outcome = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: default_options(),
            budget: tiny_matching,
        })
        .expect("cut audits should still produce outcomes");

        assert_eq!(outcome.verdict, Verdict::Inconclusive);
        assert_eq!(
            outcome.reasons,
            vec![InconclusiveReason::WorkBudgetExceeded]
        );
        assert_eq!(outcome.totals.unanalyzed_bytes, block.len() as u64);
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn index_memory_failures_should_surface_as_the_early_envelope_error() {
        let base = AuditedTree::new(&[(b"src.txt", b"a-unique-removed-payload\n")]);
        let head = AuditedTree::new(&[(b"bulk.txt", vec![b's'; 256].as_slice())]);
        let tiny_index = budget_with(u64::MAX, u64::MAX, u64::MAX, 48, u64::MAX, 64);

        let error = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: default_options(),
            budget: tiny_index,
        })
        .expect_err("index memory must fail closed before any region matches");

        assert!(matches!(
            error,
            CoreError::ResourceLimitExceeded {
                resource: "conservation_index_memory",
                actual: 80,
                limit: 48
            }
        ));
    }

    #[test]
    fn retained_region_overflow_should_error_before_any_region_matches() {
        let base = AuditedTree::new(&[
            (b"one.txt", b"first-deleted-payload-aaaaaaaaaa\n"),
            (b"two.txt", b"second-deleted-payload-bbbbbbbbbb\n"),
        ]);
        let head = AuditedTree::new(&[]);
        let cap_one = budget_with(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 1, 64);

        let error = audit_conservation(ConservationInput {
            base: &base.files(),
            head: &head.files(),
            options: default_options(),
            budget: cap_one,
        })
        .expect_err("retaining the second region should exceed the cap");

        assert!(matches!(
            error,
            CoreError::ResourceLimitExceeded {
                resource: "retained_regions",
                actual: 2,
                limit: 1
            }
        ));
    }

    #[test]
    fn equal_digests_should_force_unchanged_even_when_bytes_diverge() {
        let shared = leaked_digest(11);
        let base = vec![audit_file(b"a.txt", b"bytes-from-the-base-tree", shared)];
        let head = vec![audit_file(b"a.txt", b"different bytes entirely!!", shared)];

        let outcome = audit_conservation(ConservationInput {
            base: &base,
            head: &head,
            options: default_options(),
            budget: unlimited_budget(),
        })
        .expect("digest ties must classify unchanged");

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.totals.removed_regions, 0);
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn identical_trees_and_empty_trees_should_pass_with_zero_totals() {
        let same = AuditedTree::new(&[(b"a.txt", b"same-content\n")]);
        let outcome = audit(&same, &same);
        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.totals.removed_bytes, 0);
        assert_eq!(outcome.totals.new_bytes, 0);
        assert_ledger_identity(&outcome);

        let empty_base = AuditedTree::new(&[]);
        let empty_head = AuditedTree::new(&[]);
        let empty = audit(&empty_base, &empty_head);
        assert_eq!(empty.verdict, Verdict::Pass);
        assert_ledger_identity(&empty);

        let fresh = audit(&empty_base, &same);
        assert_eq!(fresh.verdict, Verdict::Pass);
        assert_eq!(fresh.totals.new_bytes, b"same-content\n".len() as u64);
        assert_ledger_identity(&fresh);
    }

    #[test]
    fn regions_below_the_index_window_should_land_in_insubstantial_residue() {
        // A 14-byte deletion sits under K = 16: it can never probe, so
        // its full length flows to insubstantial_residue and cannot fail.
        const GONE: &[u8] = b"short-and-gone";
        assert_eq!(GONE.len(), 14);
        let base = AuditedTree::new(&[(b"tiny.txt", GONE)]);
        let head = AuditedTree::new(&[(b"other.txt", b"nothing-to-see-here\n")]);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.totals.removed_regions, 1);
        assert_eq!(
            outcome.totals.insubstantial_residue_bytes,
            GONE.len() as u64
        );
        assert!(outcome.records.is_empty());
        assert_ledger_identity(&outcome);
    }

    #[test]
    fn binary_files_should_report_null_line_ranges_while_still_matching() {
        let mut binary_content = b"BIN\x00PAYLOAD-CONTENT-1234567890".to_vec();
        binary_content.extend_from_slice(&[0xff, 0xfe]);

        let base = AuditedTree::new(&[(b"assets/blob.bin", binary_content.as_slice())]);
        let head = AuditedTree::new(&[(b"assets/copy.bin", binary_content.as_slice())]);

        let outcome = audit(&base, &head);

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.records.len(), 1);
        assert_eq!(outcome.records[0].source_line_range, None);
        assert_eq!(outcome.records[0].destinations[0].line_range, None);
        assert_ledger_identity(&outcome);

        let text = AuditedTree::new(&[(b"t/a.txt", b"line-one\nline-two\n")]);
        let text_head = AuditedTree::new(&[(b"t/b.txt", b"line-one\nline-two\n")]);
        let text_outcome = audit(&text, &text_head);
        assert_eq!(text_outcome.records[0].source_line_range, Some((1, 2)));
        assert_eq!(
            text_outcome.records[0].destinations[0].line_range,
            Some((1, 2))
        );
    }

    #[test]
    fn unsorted_inputs_should_produce_identical_outcomes_to_sorted_inputs() {
        let payload: &[u8] = b"order-independent-payload-XYZ-987654321";
        let host: &[u8] = b"context|order-independent-payload-XYZ-987654321|done\n";
        let context: &[u8] = b"shared-context\n";

        let sorted_base =
            AuditedTree::new(&[(b"a-first.txt", context), (b"z-second.txt", payload)]);
        let reversed_base =
            AuditedTree::new(&[(b"z-second.txt", payload), (b"a-first.txt", context)]);
        let head = AuditedTree::new(&[(b"m-middle.txt", host), (b"a-first.txt", context)]);

        let sorted_outcome = audit(&sorted_base, &head);
        let reversed_outcome = audit(&reversed_base, &head);

        assert_eq!(sorted_outcome.verdict, Verdict::Pass);
        assert_eq!(sorted_outcome, reversed_outcome);
        assert_ledger_identity(&sorted_outcome);
    }

    #[test]
    fn report_consistency_should_accept_the_engine_shaped_artifacts() {
        let head = vec![leak_file(b"h.bin", vec![0_u8; 100])];
        let mut consumption = vec![ConsumptionLog::default()];
        consumption[0].consume_for_tests(10, 30);
        let spans = vec![NewContentSpan {
            path: b"h.bin".to_vec(),
            byte_range: (0, 100),
            length: 80,
            sha256: sha256_digest(&head[0].bytes[0..100]),
        }];

        assert!(report_consistency_holds(&consumption, &spans, &head));
    }

    #[test]
    fn report_consistency_should_reject_span_lengths_that_ignore_consumed_overlaps() {
        // Artificial violation: the span reports its full length even though
        // 20 consumed bytes lie inside it, so `consumed ∪ new` would
        // double-count those bytes.
        let head = vec![leak_file(b"h.bin", vec![0_u8; 100])];
        let mut consumption = vec![ConsumptionLog::default()];
        consumption[0].consume_for_tests(10, 30);
        let spans = vec![NewContentSpan {
            path: b"h.bin".to_vec(),
            byte_range: (0, 100),
            length: 100,
            sha256: sha256_digest(&head[0].bytes[0..100]),
        }];

        assert!(!report_consistency_holds(&consumption, &spans, &head));
    }

    #[test]
    fn report_consistency_should_reject_out_of_bounds_spans_and_intervals() {
        let head = vec![leak_file(b"h.bin", vec![0_u8; 100])];
        let consumption = vec![ConsumptionLog::from_intervals_for_tests(vec![(90, 101)])];
        let out_of_bounds_interval = report_consistency_holds(&consumption, &[], &head);
        assert!(!out_of_bounds_interval);

        let clean_consumption = vec![ConsumptionLog::default()];
        let spans = vec![NewContentSpan {
            path: b"h.bin".to_vec(),
            byte_range: (0, 101),
            length: 101,
            sha256: sha256_digest(&head[0].bytes[0..100]),
        }];
        assert!(!report_consistency_holds(&clean_consumption, &spans, &head));
    }

    #[test]
    fn report_consistency_should_reject_overlapping_consumed_intervals() {
        // Artificial violation via raw intervals: consume's merge-on-insert
        // would repair this, so the raw constructor is required to reach it.
        let head = vec![leak_file(b"h.bin", vec![0_u8; 100])];
        let consumption = vec![ConsumptionLog::from_intervals_for_tests(vec![
            (10, 20),
            (15, 25),
        ])];

        assert!(!report_consistency_holds(&consumption, &[], &head));
    }

    #[test]
    fn report_consistency_should_reject_unknown_span_paths() {
        let head = vec![leak_file(b"h.bin", vec![0_u8; 100])];
        let consumption = vec![ConsumptionLog::default()];
        let spans = vec![NewContentSpan {
            path: b"missing.bin".to_vec(),
            byte_range: (0, 10),
            length: 10,
            sha256: sha256_digest(b"0123456789"),
        }];

        assert!(!report_consistency_holds(&consumption, &spans, &head));
    }

    fn join(parts: &[&[u8]]) -> Vec<u8> {
        let mut joined = Vec::new();
        for part in parts {
            joined.extend_from_slice(part);
        }
        joined
    }
}
