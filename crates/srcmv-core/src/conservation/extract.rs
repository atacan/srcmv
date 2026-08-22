//! Removed-region extraction from modified and deleted files.
//!
//! Implements §8.1 with the §8.7 sequencing pinned by the plan: every
//! gap-derived modified-pair region is compared against its paired insertion
//! context under line-edge normalization immediately after gap trimming,
//! before any probing or length-based short-circuit can see it. Deleted-file
//! regions carry no paired context and are never formatting-classified.
//!
//! Extraction work accounting (documented model, charged against the shared
//! extraction pool): every byte inspected by trims, line indexing, content
//! hashing, gap trims, normalization passes, digests, and whole-file reads
//! costs one unit, and LIS selection costs [`LIS_UNIT_PER_ANCHOR`] units per
//! selected-anchor candidate. The per-pair anchor-pass soft cap compares the
//! same units; breaching degrades only that pair to the coarse fallback while
//! its spent units stay deducted from the pool.
//!
//! Post-matching assembly digests — the whole-file digests synthesized for
//! unanalyzed placeholders here and any new-content span digests downstream —
//! are deliberately outside the work-unit pools: they are deterministic,
//! bounded by the retained-region cap and the Phase 3 report caps, and they
//! run only after all verdict-relevant work has completed.

use std::collections::HashMap;

use crate::{CoreError, LineIndex, Sha256Digest};

use super::{
    AuditFile, ConservationBudget, WorkPool, format::formatting_equivalent, index::mix_bytes,
    sha256_digest,
};

/// Work units charged per candidate during LIS anchor selection.
const LIS_UNIT_PER_ANCHOR: u64 = 16;

/// Head-side byte range paired with a removed region for §8.7 comparison and
/// §8.8 insertion accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PairedContext {
    /// Rank of the hosting head file in path-sorted order.
    pub head_file: usize,
    /// Inclusive start offset inside the head file.
    pub start: u64,
    /// Exclusive end offset inside the head file.
    pub end: u64,
}

/// Provenance of an extracted removal region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegionKind {
    /// Derived from one bracketed gap or from the coarse fallback; carries
    /// the paired insertion context when its after-side remainder was
    /// nonempty.
    GapDerived {
        /// Paired after-side remainder, absent when it was empty.
        context: Option<PairedContext>,
    },
    /// Whole deleted file; never formatting-classified.
    WholeDeletedFile,
    /// Synthesized ledger entry for input the budgets never reached.
    UnanalyzedPlaceholder,
}

/// One maximal removed byte span with presentation data derived later.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedRegion {
    /// Rank of the source file in path-sorted base order.
    pub base_file: usize,
    /// Inclusive start offset inside the base file.
    pub start: u64,
    /// Exclusive end offset inside the base file.
    pub end: u64,
    /// SHA-256 of the exact removed bytes.
    pub digest: Sha256Digest,
    /// Set at extraction time by the §8.7 formatting gate.
    pub formatting_only: bool,
    /// Provenance and paired context.
    pub kind: RegionKind,
}

impl ExtractedRegion {
    /// Returns the region length in bytes.
    #[must_use]
    pub(crate) const fn length(&self) -> u64 {
        self.end - self.start
    }
}

/// Everything extraction produced for one audit run.
#[derive(Debug, Default)]
pub(crate) struct ExtractionOutput {
    /// Regions in modified-pair then deleted-file path order, including
    /// synthesized placeholders for inputs the pool never reached.
    pub regions: Vec<ExtractedRegion>,
    /// After-side remainders and created-file spans as
    /// `(head file rank, start, end)` for §8.8 new-content accounting.
    pub insertion_spans: Vec<(usize, u64, u64)>,
    /// Whether the extraction pool ran dry before every input was processed.
    pub pool_exhausted: bool,
}

enum PairAbort {
    PoolExhausted,
}

/// What one modified pair contributed to the run.
struct PairOutput {
    regions: Vec<ExtractedRegion>,
    insertion_spans: Vec<(usize, u64, u64)>,
}

/// Charges units against both the shared pool and the per-pair soft cap.
struct PairBudget<'p> {
    pool: &'p mut WorkPool,
    cap: u64,
    spent: u64,
}

impl<'p> PairBudget<'p> {
    fn new(pool: &'p mut WorkPool, cap: u64) -> Self {
        Self {
            pool,
            cap,
            spent: 0,
        }
    }

    fn spend(&mut self, units: u64) -> Result<(), PairAbort> {
        self.spent = self.spent.saturating_add(units);
        if !self.pool.spend(units) {
            return Err(PairAbort::PoolExhausted);
        }
        Ok(())
    }

    const fn over_cap(&self) -> bool {
        self.spent > self.cap
    }
}

struct PairExtractor<'a> {
    base_file: usize,
    head_file: usize,
    /// Full before-file bytes; gap coordinates stay middle-relative.
    before: &'a [u8],
    /// Full after-file bytes.
    after: &'a [u8],
    /// Trimmed before-middle bounds in absolute file coordinates.
    mid_before: (u64, u64),
    /// Trimmed after-middle bounds in absolute file coordinates.
    mid_after: (u64, u64),
    accountant: PairBudget<'a>,
    regions: Vec<ExtractedRegion>,
    insertion_spans: Vec<(usize, u64, u64)>,
}

impl PairExtractor<'_> {
    /// Trims one gap pair and emits its removal region plus insertion span.
    ///
    /// The before-side remainder becomes the candidate region; the after-side
    /// remainder feeds the insertion-span set whether or not a region exists.
    /// The §8.7 gate runs here, before any matching stage can observe the
    /// region, exactly as sequenced by the plan.
    fn emit_gap(
        &mut self,
        b_start: u64,
        b_end: u64,
        a_start: u64,
        a_end: u64,
    ) -> Result<(), PairAbort> {
        let before_gap = &self.before
            [(self.mid_before.0 + b_start) as usize..(self.mid_before.0 + b_end) as usize];
        let after_gap =
            &self.after[(self.mid_after.0 + a_start) as usize..(self.mid_after.0 + a_end) as usize];
        self.accountant
            .spend(bytes_to_units(before_gap.len() + after_gap.len()))?;

        let prefix = common_prefix_len(before_gap, after_gap);
        let suffix_cap = before_gap.len().min(after_gap.len()).saturating_sub(prefix);
        let suffix = common_suffix_len(before_gap, after_gap, suffix_cap);

        let region_start = b_start + prefix as u64;
        let region_end = b_end - suffix as u64;
        let ctx_start = a_start + prefix as u64;
        let ctx_end = a_end - suffix as u64;

        // Every gap-derived region annotates its counterpart, even when the
        // after-side remainder is empty (pure EOL conversions); the
        // insertion-span set itself only records nonempty remainders.
        let context = Some(PairedContext {
            head_file: self.head_file,
            start: self.mid_after.0 + ctx_start,
            end: self.mid_after.0 + ctx_end,
        });
        if ctx_end > ctx_start {
            self.insertion_spans.push((
                self.head_file,
                self.mid_after.0 + ctx_start,
                self.mid_after.0 + ctx_end,
            ));
        }

        if region_end > region_start {
            let region_bytes = &self.before[(self.mid_before.0 + region_start) as usize
                ..(self.mid_before.0 + region_end) as usize];
            let ctx_bytes = &self.after
                [(self.mid_after.0 + ctx_start) as usize..(self.mid_after.0 + ctx_end) as usize];
            self.accountant
                .spend(bytes_to_units(region_bytes.len() + ctx_bytes.len()))?;
            let formatting_only = formatting_equivalent(region_bytes, ctx_bytes);
            self.accountant.spend(bytes_to_units(region_bytes.len()))?;
            let digest = sha256_digest(region_bytes);
            self.regions.push(ExtractedRegion {
                base_file: self.base_file,
                start: self.mid_before.0 + region_start,
                end: self.mid_before.0 + region_end,
                digest,
                formatting_only,
                kind: RegionKind::GapDerived { context },
            });
        }
        Ok(())
    }

    /// Runs the full §8.1 pipeline for one modified pair.
    ///
    /// Falls back to the single sentinel-spanning gap when anchoring found no
    /// usable brackets or breached the anchor-pass effort cap.
    fn run(mut self) -> Result<PairOutput, PairAbort> {
        // Step 1: trim the common byte prefix and suffix of the changed pair.
        self.accountant.spend(bytes_to_units(
            self.before.len().saturating_add(self.after.len()),
        ))?;
        let prefix = common_prefix_len(self.before, self.after);
        let suffix_cap = self
            .before
            .len()
            .min(self.after.len())
            .saturating_sub(prefix);
        let suffix = common_suffix_len(self.before, self.after, suffix_cap);
        self.mid_before = (prefix as u64, (self.before.len() - suffix) as u64);
        self.mid_after = (prefix as u64, (self.after.len() - suffix) as u64);
        let before_middle = &self.before[self.mid_before.0 as usize..self.mid_before.1 as usize];
        let after_middle = &self.after[self.mid_after.0 as usize..self.mid_after.1 as usize];

        self.accountant
            .spend(bytes_to_units(before_middle.len() + after_middle.len()))?;
        let before_lines = line_contents(before_middle);
        let after_lines = line_contents(after_middle);

        let before_hashes = hashed_lines(&mut self.accountant, before_middle, &before_lines)?;
        let after_hashes = hashed_lines(&mut self.accountant, after_middle, &after_lines)?;

        let mut before_counts: HashMap<u64, u32> = HashMap::new();
        for &hash in &before_hashes {
            *before_counts.entry(hash).or_default() += 1;
        }
        let mut after_counts: HashMap<u64, u32> = HashMap::new();
        for &hash in &after_hashes {
            *after_counts.entry(hash).or_default() += 1;
        }

        let mut anchors: Vec<(u64, u64)> = Vec::new();
        for (after_line, &hash) in after_hashes.iter().enumerate() {
            if after_counts.get(&hash) != Some(&1) {
                continue;
            }
            if before_counts.get(&hash) == Some(&1) {
                let before_line = before_hashes
                    .iter()
                    .position(|&candidate| candidate == hash)
                    .expect("counted unique hash must exist");
                anchors.push((before_line as u64, after_line as u64));
            }
        }
        anchors.sort_unstable();

        let fallback = self.accountant.over_cap() || anchors.is_empty();
        if fallback {
            self.emit_gap(0, before_middle.len() as u64, 0, after_middle.len() as u64)?;
        } else {
            self.accountant
                .spend(LIS_UNIT_PER_ANCHOR.saturating_mul(anchors.len() as u64))?;
            let selected = patience_select(&anchors);
            let mut cursor_before = 0_u64;
            let mut cursor_after = 0_u64;
            for slot in selected {
                let (before_line, after_line) = anchors[slot];
                let (b_start, b_content_end) = before_lines[before_line as usize];
                let (a_start, a_content_end) = after_lines[after_line as usize];
                self.emit_gap(cursor_before, b_start, cursor_after, a_start)?;
                cursor_before = b_content_end;
                cursor_after = a_content_end;
            }
            self.emit_gap(
                cursor_before,
                before_middle.len() as u64,
                cursor_after,
                after_middle.len() as u64,
            )?;
        }

        Ok(PairOutput {
            regions: self.regions,
            insertion_spans: self.insertion_spans,
        })
    }
}

fn hashed_lines(
    accountant: &mut PairBudget<'_>,
    middle: &[u8],
    lines: &[(u64, u64)],
) -> Result<Vec<u64>, PairAbort> {
    let mut hashes = Vec::with_capacity(lines.len());
    for &(start, end) in lines {
        let content = &middle[start as usize..end as usize];
        accountant.spend(bytes_to_units(content.len()))?;
        hashes.push(mix_bytes(content.trim_ascii()));
    }
    Ok(hashes)
}

fn bytes_to_units(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Extracts removal regions for every modified pair and deleted file.
///
/// Inputs are consumed in path-sorted order. When the extraction pool runs
/// dry, remaining inputs (including a partially processed pair) become
/// [`RegionKind::UnanalyzedPlaceholder`] regions covering their whole base
/// file, keeping the §9.2 identity meaningful without pretending those bytes
/// were analyzed. Bracketed gaps never fall back regardless of size; the
/// fallback applies only to zero-anchor middles and effort-cap breaches.
///
/// # Errors
///
/// Returns [`CoreError::ResourceLimitExceeded`] when the number of retained
/// regions would exceed `budget.retained_regions()`.
pub(crate) fn extract_all(
    base: &[AuditFile<'_>],
    head: &[AuditFile<'_>],
    modified: &[(usize, usize)],
    deleted: &[usize],
    budget: &ConservationBudget,
    pool: &mut WorkPool,
) -> Result<ExtractionOutput, CoreError> {
    let mut output = ExtractionOutput::default();
    let retained_cap = budget.retained_regions();
    let mut exhausted = false;

    for &(base_index, head_index) in modified {
        if exhausted {
            push_placeholder(&mut output.regions, base, base_index, retained_cap)?;
            continue;
        }
        let extractor = PairExtractor {
            base_file: base_index,
            head_file: head_index,
            before: base[base_index].bytes,
            after: head[head_index].bytes,
            mid_before: (0, 0),
            mid_after: (0, 0),
            accountant: PairBudget::new(pool, budget.anchor_effort_cap_per_pair()),
            regions: Vec::new(),
            insertion_spans: Vec::new(),
        };
        match extractor.run() {
            Ok(pair_output) => {
                for region in pair_output.regions {
                    push_region(&mut output.regions, region, retained_cap)?;
                }
                output.insertion_spans.extend(pair_output.insertion_spans);
            }
            Err(PairAbort::PoolExhausted) => {
                exhausted = true;
                push_placeholder(&mut output.regions, base, base_index, retained_cap)?;
            }
        }
    }

    for &base_index in deleted {
        let file = base[base_index];
        if file.bytes.is_empty() {
            continue;
        }
        if exhausted {
            push_placeholder(&mut output.regions, base, base_index, retained_cap)?;
            continue;
        }
        if !pool.spend(bytes_to_units(file.bytes.len())) {
            exhausted = true;
            push_placeholder(&mut output.regions, base, base_index, retained_cap)?;
            continue;
        }
        let digest = sha256_digest(file.bytes);
        push_region(
            &mut output.regions,
            ExtractedRegion {
                base_file: base_index,
                start: 0,
                end: bytes_to_units(file.bytes.len()),
                digest,
                formatting_only: false,
                kind: RegionKind::WholeDeletedFile,
            },
            retained_cap,
        )?;
    }

    output.pool_exhausted = exhausted;
    Ok(output)
}

fn push_region(
    regions: &mut Vec<ExtractedRegion>,
    region: ExtractedRegion,
    retained_cap: u64,
) -> Result<(), CoreError> {
    let next_count = regions.len() as u64 + 1;
    if next_count > retained_cap {
        return Err(CoreError::ResourceLimitExceeded {
            resource: "retained_regions",
            actual: next_count,
            limit: retained_cap,
        });
    }
    regions.push(region);
    Ok(())
}

fn push_placeholder(
    regions: &mut Vec<ExtractedRegion>,
    base: &[AuditFile<'_>],
    base_index: usize,
    retained_cap: u64,
) -> Result<(), CoreError> {
    let bytes = base[base_index].bytes;
    if bytes.is_empty() {
        return Ok(());
    }
    push_region(
        regions,
        ExtractedRegion {
            base_file: base_index,
            start: 0,
            end: bytes_to_units(bytes.len()),
            digest: sha256_digest(bytes),
            formatting_only: false,
            kind: RegionKind::UnanalyzedPlaceholder,
        },
        retained_cap,
    )
}

const fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let mut count = 0_usize;
    while count < a.len() && count < b.len() && a[count] == b[count] {
        count += 1;
    }
    count
}

fn common_suffix_len(a: &[u8], b: &[u8], cap: usize) -> usize {
    let max = cap.min(a.len()).min(b.len());
    let mut count = 0_usize;
    while count < max && a[a.len() - 1 - count] == b[b.len() - 1 - count] {
        count += 1;
    }
    count
}

/// Returns the terminator length ending the line whose inclusive boundary is
/// `inclusive_end`, or zero for an unterminated final line.
fn terminator_length(bytes: &[u8], inclusive_end: u64) -> u64 {
    let end = inclusive_end as usize;
    match end.checked_sub(1).and_then(|index| bytes.get(index)) {
        Some(b'\n') => {
            if end >= 2 && bytes[end - 2] == b'\r' {
                2
            } else {
                1
            }
        }
        Some(b'\r') => 1,
        _ => 0,
    }
}

/// Splits `bytes` into terminator-exclusive logical line content spans under
/// the reused `LineIndex` semantics (LF, CRLF, lone CR; unterminated final
/// suffix is a line).
fn line_contents(bytes: &[u8]) -> Vec<(u64, u64)> {
    let Ok(index) = LineIndex::from_bytes_with_limits(bytes, u64::MAX, u64::MAX) else {
        return Vec::new();
    };
    let mut spans = Vec::with_capacity(index.line_count() as usize);
    for line in 1..=index.line_count() {
        let (Some(start), Some(end)) = (index.line_start(line), index.line_end(line)) else {
            continue;
        };
        spans.push((start, end - terminator_length(bytes, end)));
    }
    spans
}

/// Selects the longest strictly increasing subsequence of after-positions,
/// breaking ties toward the lexicographically smallest sequence of
/// after-positions via standard patience-stack reconstruction.
///
/// `anchors` must be sorted ascending by before-position with distinct
/// before- and after-values, which uniqueness anchoring guarantees. Returns
/// indices into `anchors`.
#[must_use]
pub(crate) fn patience_select(anchors: &[(u64, u64)]) -> Vec<usize> {
    // Piles keep their full placement history ordered by ascending anchor
    // index and therefore descending after-value, because every replacement
    // pushes a smaller-or-equal tail onto the leftmost valid pile.
    let mut piles: Vec<Vec<(u64, usize)>> = Vec::new();
    for (anchor_index, &(_, after)) in anchors.iter().enumerate() {
        let target =
            piles.partition_point(|pile| pile.last().is_some_and(|&(value, _)| value < after));
        if target == piles.len() {
            piles.push(Vec::new());
        }
        piles[target].push((after, anchor_index));
    }

    let mut chosen = Vec::with_capacity(piles.len());
    let mut previous_value = u64::MAX;
    let mut previous_position = usize::MAX;
    for level in (0..piles.len()).rev() {
        let pile = &piles[level];
        // Entries usable at this level form the intersection of "value <
        // previous_value" (a suffix; values descend) and "index <
        // previous_position" (a prefix; indexes ascend). Taking the last such
        // entry chooses the smallest feasible after-value, reconstructing the
        // lexicographically smallest forward sequence.
        let value_start = pile.partition_point(|&(value, _)| value >= previous_value);
        let index_end = pile.partition_point(|&(_, position)| position < previous_position);
        if index_end <= value_start {
            debug_assert!(false, "patience invariant violated");
            break;
        }
        let (value, position) = pile[index_end - 1];
        chosen.push(position);
        previous_value = value;
        previous_position = position;
    }
    chosen.reverse();
    chosen
}

#[cfg(test)]
mod tests {
    use super::WorkPool;
    use super::{RegionKind, line_contents, patience_select};
    use crate::conservation::test_support::{AuditedTree, budget_with, unlimited_budget};

    fn extract(base: AuditedTree, head: AuditedTree) -> Vec<(String, u64, u64, bool)> {
        let base_files = base.files();
        let head_files = head.files();
        let modified: Vec<(usize, usize)> = (0..base_files.len())
            .filter(|&index| {
                base_files[index].digest != head_files[index].digest
                    || base_files[index].bytes != head_files[index].bytes
            })
            .map(|index| (index, index))
            .collect();
        let mut pool = WorkPool::new(unlimited_budget().extraction_work_units());
        let output = super::extract_all(
            &base_files,
            &head_files,
            &modified,
            &[],
            &unlimited_budget(),
            &mut pool,
        )
        .expect("test extraction should succeed");
        output
            .regions
            .iter()
            .map(|region| {
                (
                    String::from_utf8_lossy(base_files[region.base_file].path).to_string(),
                    region.start,
                    region.end,
                    region.formatting_only,
                )
            })
            .collect()
    }

    #[test]
    fn extraction_should_produce_exact_expected_regions_for_known_diffs() {
        let x: &[u8] = b"UNIQUE-PAYLOAD-BLOCK-X";
        let y: &[u8] = b"UNIQUE-PAYLOAD-BLOCK-Y";
        let base_bytes = join(&[
            b"AA\n".as_slice(),
            x,
            b"\nMID\n".as_slice(),
            y,
            b"\nZZ\n".as_slice(),
        ]);
        let head_bytes: &[u8] = b"AA\n\nMID\n\nZZ\n";
        let base = AuditedTree::new(&[(b"f0.txt", &base_bytes)]);
        let head = AuditedTree::new(&[(b"f0.txt", head_bytes)]);

        let regions = extract(base, head);

        // Both gaps sit between unique sentinels, so each block surfaces as
        // its own region trimmed to exactly the removed bytes.
        let y_start = 3 + x.len() as u64 + 5;
        assert_eq!(
            regions,
            vec![
                ("f0.txt".to_owned(), 3, 3 + x.len() as u64, false),
                (
                    "f0.txt".to_owned(),
                    y_start,
                    y_start + y.len() as u64,
                    false
                )
            ]
        );
    }

    #[test]
    fn extraction_should_fall_back_when_no_anchor_pairs_exist() {
        // Every before-middle line repeats, so nothing is unique in both
        // middles and the entire before-middle collapses into one region.
        let base = AuditedTree::new(&[(b"f0.txt", b"x\nx\nrest-of-line\n")]);
        let head = AuditedTree::new(&[(b"f0.txt", b"\n\nrest-of-line\n")]);

        let regions = extract(base, head);

        assert_eq!(regions, vec![("f0.txt".to_owned(), 0, 3, false)]);
    }

    #[test]
    fn extraction_should_fall_back_when_the_anchor_effort_cap_is_breached() {
        let payload: &[u8] = b"unique-block-one-XYZ|unique-block-two-QRS";
        let base_bytes = join(&[b"top\n".as_slice(), payload, b"\nbottom\n".as_slice()]);
        let base = AuditedTree::new(&[(b"f0.txt", &base_bytes)]);
        let head = AuditedTree::new(&[(b"f0.txt", b"top\n\nbottom\n")]);
        let capped = budget_with(u64::MAX, 1, u64::MAX, u64::MAX, u64::MAX, 64);
        let mut pool = WorkPool::new(capped.extraction_work_units());

        let output = super::extract_all(
            &base.files(),
            &head.files(),
            &[(0, 0)],
            &[],
            &capped,
            &mut pool,
        )
        .expect("capped extraction should still succeed");

        // One coarse region spanning the whole pre-trim middle instead of
        // per-gap pieces; the shared separator trims into the suffix.
        assert_eq!(output.regions.len(), 1);
        assert_eq!(
            (output.regions[0].start, output.regions[0].end),
            (4, base_bytes.len() as u64 - 8)
        );
    }

    #[test]
    fn sparse_anchors_should_keep_large_bracketed_gaps_fine_grained() {
        let a: &[u8] = b"BLOCK-AAAAAAAAAAAAAAAAAAAA";
        let b: &[u8] = b"BLOCK-BBBBBBBBBBBBBBBBBBBB";
        let base_bytes = join(&[
            b"s1\n".as_slice(),
            a,
            b"\nmiddle-keeper\n".as_slice(),
            b,
            b"\ns3\n".as_slice(),
        ]);
        let base = AuditedTree::new(&[(b"f0.txt", &base_bytes)]);
        let head = AuditedTree::new(&[(b"f0.txt", b"s1\n\nmiddle-keeper\n\ns3\n")]);

        let regions = extract(base, head);

        assert_eq!(regions.len(), 2, "two separate gaps, never merged");
        assert_eq!(regions[0].2 - regions[0].1, a.len() as u64);
        assert_eq!(regions[1].2 - regions[1].1, b.len() as u64);
    }

    #[test]
    fn lis_selection_should_break_ties_toward_smallest_after_positions() {
        // Four maximum-length choices exist; the after-value sequence [0, 2]
        // is lexicographically smallest among them.
        let anchors = [(0, 1), (1, 0), (2, 3), (3, 2)];
        assert_eq!(patience_select(&anchors), vec![1, 3]);

        // Same-file reorder: both singletons tie and the smaller after
        // position wins, matching the frozen reorder fixture trace.
        let swapped = [(0, 1), (1, 0)];
        assert_eq!(patience_select(&swapped), vec![1]);
    }

    #[test]
    fn lis_selection_should_keep_every_anchor_of_an_increasing_sequence() {
        let anchors = [(0, 0), (1, 1), (2, 2), (3, 5)];
        assert_eq!(patience_select(&anchors), vec![0, 1, 2, 3]);
    }

    #[test]
    fn line_splitting_should_handle_crlf_cr_lf_mixes_and_unterminated_tails() {
        assert_eq!(
            line_contents(b"a\r\nbravo\rc\nd"),
            vec![(0, 1), (3, 8), (9, 10), (11, 12)]
        );
        assert!(line_contents(b"").is_empty());
        assert_eq!(line_contents(b"\n"), vec![(0, 0)]);
        assert_eq!(line_contents(b"tail"), vec![(0, 4)]);
    }

    #[test]
    fn unterminated_tail_removals_should_extract_exactly() {
        let base = AuditedTree::new(&[(b"f0.txt", b"A\nGONE-WITHOUT-NEWLINE")]);
        let head = AuditedTree::new(&[(b"f0.txt", b"A\n")]);

        let regions = extract(base, head);

        assert_eq!(regions, vec![("f0.txt".to_owned(), 2, 22, false)]);
    }

    #[test]
    fn deleted_files_should_contribute_one_whole_file_region_each() {
        let base = AuditedTree::new(&[(b"gone.txt", b"whole\nfile\n"), (b"kept.txt", b"kept\n")]);
        let head = AuditedTree::new(&[(b"kept.txt", b"kept\n")]);
        let base_files = base.files();
        let head_files = head.files();
        let mut pool = WorkPool::new(u64::MAX);

        let output = super::extract_all(
            &base_files,
            &head_files,
            &[(1, 0)],
            &[0],
            &unlimited_budget(),
            &mut pool,
        )
        .expect("extraction should succeed");

        assert_eq!(output.regions.len(), 1);
        assert_eq!((output.regions[0].start, output.regions[0].end), (0, 11));
        assert!(matches!(
            output.regions[0].kind,
            RegionKind::WholeDeletedFile
        ));
        assert!(!output.pool_exhausted);
    }

    #[test]
    fn empty_deleted_files_should_contribute_nothing_and_pure_insertions_yield_spans() {
        let base = AuditedTree::new(&[(b"empty.txt", b""), (b"grown.txt", b"tiny")]);
        let head = AuditedTree::new(&[
            (b"empty.txt", b""),
            (b"grown.txt", b"tiny-and-now-much-larger"),
        ]);
        let mut pool = WorkPool::new(u64::MAX);

        let output = super::extract_all(
            &base.files(),
            &head.files(),
            &[(1, 1)],
            &[0],
            &unlimited_budget(),
            &mut pool,
        )
        .expect("extraction should succeed");

        assert!(output.regions.is_empty());
        assert_eq!(output.insertion_spans, vec![(1, 4, 24)]);
    }

    #[test]
    fn lone_carriage_return_gaps_should_classify_formatting_at_extraction_time() {
        // Pure CRLF->LF conversion: each removed gap is a single CR byte and
        // qualifies through W even though it is far shorter than K, proving
        // the gate runs before any length-based short-circuit.
        let base = AuditedTree::new(&[(b"f0.txt", b"alpha\r\nbravo\r\ncharlie\r\n")]);
        let head = AuditedTree::new(&[(b"f0.txt", b"alpha\nbravo\ncharlie\n")]);

        let regions = extract(base, head);

        assert_eq!(regions.len(), 3);
        assert!(regions.iter().all(|&(_, _, _, formatting)| formatting));
        assert!(regions.iter().all(|&(_, start, end, _)| end - start == 1));
    }

    #[test]
    fn pool_exhaustion_should_synthesize_placeholders_for_unreached_inputs() {
        let base = AuditedTree::new(&[
            (b"f0.txt", b"changed-payload-A\n"),
            (b"f1.txt", b"deleted-whole-file\n"),
        ]);
        let head = AuditedTree::new(&[(b"f0.txt", b"rewritten\n")]);
        let dry = budget_with(0, u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64);
        let mut pool = WorkPool::new(dry.extraction_work_units());

        let output = super::extract_all(
            &base.files(),
            &head.files(),
            &[(0, 0)],
            &[1],
            &dry,
            &mut pool,
        )
        .expect("placeholder synthesis should not fail");

        assert!(output.pool_exhausted);
        assert_eq!(output.regions.len(), 2);
        assert!(
            output
                .regions
                .iter()
                .all(|region| region.kind == RegionKind::UnanalyzedPlaceholder)
        );
        assert!(output.insertion_spans.is_empty());
    }

    fn join(parts: &[&[u8]]) -> Vec<u8> {
        let mut joined = Vec::new();
        for part in parts {
            joined.extend_from_slice(part);
        }
        joined
    }
}
