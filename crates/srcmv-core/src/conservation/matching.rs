//! Exact whole-region matching and fragment matching with one-to-one
//! consumption accounting.
//!
//! Implements §8.3, §8.5, and §8.6. Regions arrive in the deterministic
//! priority order (longer first, ties by source path bytes then start
//! offset). Exact matching probes the first cold window scanning forward,
//! verifies candidates in ascending `(path, offset)` order, and rejects any
//! candidate whose span intersects an already-consumed interval. After an
//! exact winner is accepted, the remaining candidates of that region keep
//! being scanned in the same ascending order: every unconsumed candidate is
//! byte-verified and a verified surplus occurrence is counted, realizing
//! §8.6's second strict-uniqueness disjunct ("more surviving candidates than
//! consumed") even when exactly one copy was removed. Fragment matching
//! walks each deferred region left to right, extending bidirectionally under
//! mirrored consumption guards and accepting the longest extension at or
//! above the fragment floor with ascending head-side start tie-breaking.
//!
//! Work-unit model (stated once here; charged against the shared matching
//! pool): every compared or verified byte costs one unit, window hashing
//! charges `K` units per window, surplus-scan verification costs one unit
//! per verified byte like any other comparison, and accepting a fragment
//! additionally charges its accepted length on top of the extension
//! comparisons that discovered it — a deliberately conservative surcharge
//! so credited bytes always cost at least twice.

use std::collections::HashMap;

use crate::Sha256Digest;

use super::{
    AuditFile, ConservationOptions, WorkPool,
    index::{CorpusIndex, INDEX_WINDOW_BYTES, mix_bytes},
    sha256_digest,
};

/// One matched head-side span.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DestinationMatch {
    /// Rank of the hosting head file in path-sorted order.
    pub head_file: usize,
    /// Inclusive start offset inside the head file.
    pub start: u64,
    /// Exclusive end offset inside the head file.
    pub end: u64,
    /// SHA-256 of the matched bytes.
    pub digest: Sha256Digest,
}

/// Piecewise result for one deferred region.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FragmentResult {
    /// Accepted fragments in left-to-right region order.
    pub destinations: Vec<DestinationMatch>,
    /// Sum of accepted fragment lengths.
    pub conserved: u64,
    /// Walked-but-unmatched bytes, classified against the substantiality
    /// floor by the ledger assembler.
    pub residue: u64,
    /// Never-walked bytes; nonzero only after a budget cut.
    pub unanalyzed: u64,
    /// Set only when a credit was actually taken (at least one accepted
    /// fragment) and the removed content existed in multiple copies, so the
    /// copy pairing relied on multiplicity counting (§8.6). Regions that
    /// never matched carry no flag.
    pub pairing_ambiguous: bool,
}

/// Per-region matching disposition, aligned with the eligible region order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Disposition {
    /// Whole region byte-verified at one exclusively consumed occurrence.
    Exact {
        /// The winning head-side occurrence.
        destination: DestinationMatch,
        /// Set when the match relied on multiplicity counting: more than one
        /// removed copy of this content existed, or a verified unconsumed
        /// surplus occurrence remained after the winner was consumed (§8.6).
        pairing_ambiguous: bool,
    },
    /// Every existing window is hot-suppressed: anchoring is impossible, so
    /// nothing may be credited or forgiven (§8.6).
    AmbiguousCommon,
    /// Piecewise acceptance with residue and unanalyzed splits.
    Fragments(FragmentResult),
    /// Matching stopped before this region ran; wholly unanalyzed.
    NotProcessed,
}

/// A region admitted to matching: non-formatting, length at least `K`.
///
/// Source identity rejoins the disposition results upstream through the slot
/// order, so only content and its digest travel here.
#[derive(Clone, Copy)]
pub(crate) struct EligibleRegion<'a> {
    /// SHA-256 of the exact removed bytes.
    pub digest: Sha256Digest,
    /// The exact removed bytes.
    pub bytes: &'a [u8],
}

/// Merged sorted consumed intervals for one head file.
///
/// Intervals stay pairwise disjoint and sorted by construction, so overlap
/// checks are binary searches. One physical occurrence can satisfy at most
/// one region, so shared boilerplate cannot be double-counted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConsumptionLog {
    intervals: Vec<(u64, u64)>,
}

impl ConsumptionLog {
    fn intersects(&self, start: u64, end: u64) -> bool {
        let index = self.intervals.partition_point(|&(low, _)| low < end);
        index > 0 && self.intervals[index - 1].1 > start
    }

    fn consume(&mut self, start: u64, end: u64) {
        debug_assert!(start < end);
        let position = self.intervals.partition_point(|&(low, _)| low < start);
        self.intervals.insert(position, (start, end));
        let mut low = position;
        if low > 0 && self.intervals[low - 1].1 >= self.intervals[low].0 {
            self.intervals[low - 1].1 = self.intervals[low - 1].1.max(self.intervals[low].1);
            self.intervals.remove(low);
            low -= 1;
        }
        while low + 1 < self.intervals.len() && self.intervals[low + 1].0 <= self.intervals[low].1 {
            self.intervals[low].1 = self.intervals[low].1.max(self.intervals[low + 1].1);
            self.intervals.remove(low + 1);
        }
    }

    fn free_extent_from(&self, start: u64) -> u64 {
        let index = self.intervals.partition_point(|&(_, high)| high <= start);
        match self.intervals.get(index) {
            Some(&(low, _)) => low.saturating_sub(start),
            None => u64::MAX,
        }
    }

    fn occupied_end_before(&self, offset: u64) -> u64 {
        let index = self.intervals.partition_point(|&(low, _)| low < offset);
        if index == 0 {
            0
        } else {
            self.intervals[index - 1].1.min(offset)
        }
    }

    /// Returns the retained intervals for invariant checks and assembly.
    pub(crate) fn intervals(&self) -> &[(u64, u64)] {
        &self.intervals
    }

    /// Consumes an interval directly for white-box fragment tests.
    #[cfg(test)]
    pub(crate) fn consume_for_tests(&mut self, start: u64, end: u64) {
        self.consume(start, end);
    }

    /// Builds a log from raw intervals so white-box consistency tests can
    /// inject violations the merge-on-consume constructor would normalize
    /// away.
    #[cfg(test)]
    pub(crate) fn from_intervals_for_tests(intervals: Vec<(u64, u64)>) -> Self {
        Self { intervals }
    }
}

/// Everything matching produced for one audit run.
#[derive(Debug, Default)]
pub(crate) struct MatchOutputs {
    /// Dispositions aligned with the eligible region input order.
    pub dispositions: Vec<Disposition>,
    /// Per-head-file consumption logs indexed by path-sorted rank.
    pub consumption: Vec<ConsumptionLog>,
}

/// Runs the exact pass followed by the fragment pass.
///
/// The exact pass realizes both §8.6 strict-uniqueness disjuncts: a region
/// whose content was removed more than once is flagged, and after accepting
/// its winner the candidate scan continues in ascending order,
/// byte-verifying each unconsumed candidate (charging work units) so a
/// verified surplus occurrence flags the record even when exactly one copy
/// was removed. Surplus candidates are counted, never consumed: they stay
/// free as escape copies for later equal regions.
///
/// When the matching pool runs dry, processing stops deterministically at
/// that point in region order: an interrupted region keeps its accepted
/// fragments and ledgers its unprocessed remainder as `unanalyzed`, and every
/// later region becomes [`Disposition::NotProcessed`].
pub(crate) fn run_matching(
    eligible: &[EligibleRegion<'_>],
    head: &[AuditFile<'_>],
    index: &CorpusIndex,
    options: &ConservationOptions,
    pool: &mut WorkPool,
    removed_multiplicity: &HashMap<[u8; 32], u64>,
) -> MatchOutputs {
    let mut outputs = MatchOutputs {
        dispositions: vec![Disposition::NotProcessed; eligible.len()],
        consumption: vec![ConsumptionLog::default(); head.len()],
    };
    let k_usize = INDEX_WINDOW_BYTES;
    let k_units = INDEX_WINDOW_BYTES as u64;
    let min_fragment = options.min_fragment_bytes();

    let mut deferred: Vec<usize> = Vec::new();
    for (slot, entry) in eligible.iter().enumerate() {
        if pool.exhausted() {
            break;
        }
        let bytes = entry.bytes;
        let mut probe: Option<(usize, u64)> = None;
        let mut scan_cut = false;
        for window in 0..=bytes.len().saturating_sub(k_usize) {
            if !pool.spend(k_units) {
                scan_cut = true;
                break;
            }
            let key = mix_bytes(&bytes[window..window + k_usize]);
            if index.cold_candidates(key).is_some() {
                // Absent keys count as cold with no candidates; only
                // hot-suppressed buckets return None.
                probe = Some((window, key));
                break;
            }
        }
        if scan_cut {
            break;
        }
        let Some((window, key)) = probe else {
            // Every existing window is hot-suppressed: anchoring is
            // impossible, so nothing may be credited or forgiven (§8.6).
            outputs.dispositions[slot] = Disposition::AmbiguousCommon;
            continue;
        };

        let candidates = index
            .cold_candidates(key)
            .expect("probe key was just checked cold");
        let mut verified_surplus = 0_u64;
        let mut accepted: Option<DestinationMatch> = None;
        let mut cut = false;
        for candidate in candidates.iter().copied() {
            let host_rank = candidate.file as usize;
            let host_bytes = head[host_rank].bytes;
            let Some(occurrence) = candidate.offset.checked_sub(window as u64) else {
                continue;
            };
            let occurrence_end = occurrence.saturating_add(bytes.len() as u64);
            if occurrence_end > host_bytes.len() as u64 {
                continue;
            }
            if outputs.consumption[host_rank].intersects(occurrence, occurrence_end) {
                continue;
            }
            if !pool.spend(bytes.len() as u64) {
                cut = true;
                break;
            }
            if &host_bytes[occurrence as usize..occurrence_end as usize] == bytes {
                if accepted.is_none() {
                    outputs.consumption[host_rank].consume(occurrence, occurrence_end);
                    accepted = Some(DestinationMatch {
                        head_file: host_rank,
                        start: occurrence,
                        end: occurrence_end,
                        digest: entry.digest,
                    });
                } else {
                    verified_surplus += 1;
                }
            }
        }

        if let Some(destination) = accepted {
            let duplicated_removed = removed_multiplicity
                .get(&entry.digest.0)
                .copied()
                .unwrap_or(0)
                > 1;
            outputs.dispositions[slot] = Disposition::Exact {
                destination,
                pairing_ambiguous: duplicated_removed || verified_surplus > 0,
            };
            if cut || pool.exhausted() {
                break;
            }
        } else if cut {
            break;
        } else {
            deferred.push(slot);
        }
    }

    for slot in deferred {
        if pool.exhausted() {
            break;
        }
        let Some(result) = match_fragments(
            &eligible[slot],
            head,
            index,
            min_fragment,
            pool,
            &mut outputs.consumption,
            removed_multiplicity,
        ) else {
            break;
        };
        outputs.dispositions[slot] = Disposition::Fragments(result);
    }

    outputs
}

/// Fragment matching for one deferred region.
///
/// Returns `None` only when the pool died before a complete cold-window scan;
/// otherwise the result carries the accepted fragments plus the residue and
/// unanalyzed splits. Advancing past a failed window equals stepping the walk
/// offset by one byte up to the next cold window, because re-probing the same
/// window with the same candidates fails identically.
fn match_fragments(
    region: &EligibleRegion<'_>,
    head: &[AuditFile<'_>],
    index: &CorpusIndex,
    min_fragment: u64,
    pool: &mut WorkPool,
    consumption: &mut [ConsumptionLog],
    removed_multiplicity: &HashMap<[u8; 32], u64>,
) -> Option<FragmentResult> {
    let bytes = region.bytes;
    let region_len = bytes.len() as u64;
    let k_usize = INDEX_WINDOW_BYTES;
    let k_units = INDEX_WINDOW_BYTES as u64;

    let mut cold: Vec<(u64, u64)> = Vec::new();
    for window in 0..=bytes.len().saturating_sub(k_usize) {
        if !pool.spend(k_units) {
            return None;
        }
        let key = mix_bytes(&bytes[window..window + k_usize]);
        if index.cold_candidates(key).is_some() {
            cold.push((window as u64, key));
        }
    }

    // The flag stays clear until a credit is actually taken: regions that
    // never matched take no credit, rely on no multiplicity counting, and
    // must carry no flag (§8.6).
    let duplicated_removed = removed_multiplicity
        .get(&region.digest.0)
        .copied()
        .unwrap_or(0)
        > 1;
    let mut result = FragmentResult::default();
    let mut pointer = 0_usize;
    let mut covered_until = 0_u64;
    while pointer < cold.len() {
        if pool.exhausted() {
            break;
        }
        let (window, key) = cold[pointer];
        let candidates = index
            .cold_candidates(key)
            .expect("cold list was collected from this index");
        let mut best: Option<(u64, u64, usize, u64)> = None;
        let mut cut = false;
        for candidate in candidates.iter().copied() {
            let host_rank = candidate.file as usize;
            let host_bytes = head[host_rank].bytes;
            let host_len = host_bytes.len() as u64;
            if candidate.offset + k_units > host_len {
                continue;
            }
            if !pool.spend(k_units) {
                cut = true;
                break;
            }
            if host_bytes[candidate.offset as usize..(candidate.offset + k_units) as usize]
                != bytes[window as usize..(window + k_units) as usize]
            {
                continue;
            }
            let log = &consumption[host_rank];
            if log.intersects(candidate.offset, candidate.offset + k_units) {
                continue;
            }
            let right_room = log
                .free_extent_from(candidate.offset)
                .min(host_len - candidate.offset);
            let head_room = candidate.offset - log.occupied_end_before(candidate.offset);
            let region_room = window - covered_until;

            let mut forward = k_units;
            while forward < right_room
                && window + forward < region_len
                && host_bytes[(candidate.offset + forward) as usize]
                    == bytes[(window + forward) as usize]
            {
                forward += 1;
                if !pool.spend(1) {
                    cut = true;
                    break;
                }
            }
            let mut backward = 0_u64;
            while !cut
                && backward < region_room
                && backward < head_room
                && host_bytes[(candidate.offset - backward - 1) as usize]
                    == bytes[(window - backward - 1) as usize]
            {
                backward += 1;
                if !pool.spend(1) {
                    cut = true;
                    break;
                }
            }
            if cut {
                break;
            }

            let total = backward + forward;
            let head_start = candidate.offset - backward;
            let improves = match best {
                None => true,
                Some((best_total, best_start, _, _)) => {
                    total > best_total || (total == best_total && head_start < best_start)
                }
            };
            if improves {
                best = Some((total, head_start, host_rank, forward));
            }
        }

        if cut {
            return Some(cut_result(result, covered_until, region_len));
        }

        let Some((total, head_start, host_rank, forward)) = best else {
            pointer += 1;
            continue;
        };
        if total < min_fragment {
            pointer += 1;
            continue;
        }
        if !pool.spend(total) {
            return Some(cut_result(result, covered_until, region_len));
        }
        let head_end = head_start + total;
        let digest = sha256_digest(&head[host_rank].bytes[head_start as usize..head_end as usize]);
        consumption[host_rank].consume(head_start, head_end);
        result.destinations.push(DestinationMatch {
            head_file: host_rank,
            start: head_start,
            end: head_end,
            digest,
        });
        result.conserved += total;
        if duplicated_removed {
            result.pairing_ambiguous = true;
        }
        covered_until = window + forward;
        while pointer < cold.len() && cold[pointer].0 < covered_until {
            pointer += 1;
        }
    }

    // Natural termination: every unaccepted byte below the walk end,
    // including interior skipped stretches, becomes residue (§8.5). Only a
    // budget cut routes never-walked bytes to `unanalyzed` (§9.2).
    result.residue = region_len - result.conserved;
    Some(result)
}

/// Splits a budget-cut walk.
///
/// On pool exhaustion everything from the end of the last accepted fragment
/// onward — including walked-but-unmatched bytes that would otherwise be
/// residue — routes to `unanalyzed`. The split is conservative in one
/// direction only: it can turn a would-be `fail` (substantial residue) into
/// `inconclusive`, never a `fail` into `pass`.
fn cut_result(mut partial: FragmentResult, covered_until: u64, region_len: u64) -> FragmentResult {
    partial.residue = covered_until.saturating_sub(partial.conserved);
    partial.unanalyzed = region_len.saturating_sub(covered_until);
    partial
}

#[cfg(test)]
mod tests {
    use super::WorkPool;
    use super::{
        ConsumptionLog, CorpusIndex, DestinationMatch, Disposition, EligibleRegion, MatchOutputs,
        run_matching,
    };
    use crate::ConservationOptions;
    use crate::conservation::test_support::{budget_with, leak_file, unlimited_budget};
    use std::collections::HashMap;

    fn options() -> ConservationOptions {
        ConservationOptions::new(32, 16, false, false).expect("default options should validate")
    }

    fn sha(bytes: &[u8]) -> crate::Sha256Digest {
        crate::conservation::sha256_digest(bytes)
    }

    fn region(bytes: &'static [u8]) -> EligibleRegion<'static> {
        EligibleRegion {
            digest: sha(bytes),
            bytes,
        }
    }

    fn run(
        eligible: &[EligibleRegion<'_>],
        head: &[crate::conservation::AuditFile<'_>],
    ) -> MatchOutputs {
        let index = CorpusIndex::build(head, &unlimited_budget()).expect("test index should build");
        let mut pool = WorkPool::new(u64::MAX);
        let multiplicity = HashMap::new();
        run_matching(eligible, head, &index, &options(), &mut pool, &multiplicity)
    }

    fn join(parts: &[&[u8]]) -> Vec<u8> {
        let mut joined = Vec::new();
        for part in parts {
            joined.extend_from_slice(part);
        }
        joined
    }

    #[test]
    fn exact_hits_should_consume_their_occurrence_and_record_the_destination() {
        let payload: &'static [u8] = b"PAYLOAD-BYTES-12345678";
        let host_bytes = join(&[b"frame|", payload, b"|frame"]);
        let head = vec![leak_file(b"dst.bin", host_bytes)];

        let outputs = run(&[region(payload)], &head);

        assert_eq!(
            outputs.dispositions[0],
            Disposition::Exact {
                destination: DestinationMatch {
                    head_file: 0,
                    start: 6,
                    end: 6 + payload.len() as u64,
                    digest: sha(payload)
                },
                pairing_ambiguous: false
            }
        );
        assert_eq!(outputs.consumption[0].intervals(), &[(6, 28)]);
    }

    #[test]
    fn whole_region_misses_should_defer_to_fragment_matching() {
        // Head carries both halves around a foreign gap, so the exact pass
        // fails and fragment matching reassembles the block piecewise.
        let half_a: &'static [u8] = b"SPLIT-ALPHA-PART--";
        let half_b: &'static [u8] = b"--SPLIT-BETA-PART--";
        let mut host = half_a.to_vec();
        host.extend_from_slice(b"xx");
        host.extend_from_slice(half_b);
        let head = vec![leak_file(b"host.bin", host)];
        let whole: &'static [u8] = b"SPLIT-ALPHA-PART----SPLIT-BETA-PART--";

        let outputs = run(&[region(whole)], &head);

        let Disposition::Fragments(result) = &outputs.dispositions[0] else {
            panic!("expected fragments, got {:?}", outputs.dispositions[0]);
        };
        assert_eq!(result.destinations.len(), 2);
        assert_eq!(result.conserved, whole.len() as u64);
        assert_eq!(result.residue, 0);
        assert_eq!(result.unanalyzed, 0);
    }

    #[test]
    fn splits_across_two_files_should_produce_cross_file_destinations() {
        let first: &'static [u8] = b"FIRST-HALF-PAYLOAD-0123456789";
        let second: &'static [u8] = b"SECONDHALFPAYLOAD-9876543210";
        let head = vec![
            leak_file(b"part-a.bin", first.to_vec()),
            leak_file(b"part-b.bin", second.to_vec()),
        ];
        let mut whole = Vec::new();
        whole.extend_from_slice(first);
        whole.extend_from_slice(second);
        let boxed: &'static [u8] = Box::leak(whole.into_boxed_slice());

        let outputs = run(&[region(boxed)], &head);

        let Disposition::Fragments(result) = &outputs.dispositions[0] else {
            panic!("expected fragments, got {:?}", outputs.dispositions[0]);
        };
        assert_eq!(result.destinations.len(), 2);
        assert_eq!(result.destinations[0].head_file, 0);
        assert_eq!(result.destinations[1].head_file, 1);
        assert_eq!(result.conserved, boxed.len() as u64);
        assert_eq!(result.residue, 0);
    }

    #[test]
    fn reordered_content_should_match_at_its_new_position() {
        let block: &'static [u8] = b"MOVED-BLOCK-CONTENT";
        let host_bytes = join(&[b"after|", block, b"|before"]);
        let head = vec![leak_file(b"reordered.bin", host_bytes)];

        let outputs = run(&[region(block)], &head);

        let Disposition::Exact { destination, .. } = outputs.dispositions[0] else {
            panic!("expected exact match after reorder");
        };
        assert_eq!(destination.start, 6);
    }

    #[test]
    fn consumed_spans_should_reject_intersecting_candidates() {
        // Two identical regions but only one surviving copy whose span is
        // flanked by foreign bytes: the second region must stay unmatched.
        let payload: &'static [u8] = b"DUPLICATED-BLOCK-123";
        let decoy_free = join(&[
            b"prefix".as_slice(),
            payload,
            b"suffix-that-extends-past-the-copy-end",
        ]);
        let head = vec![leak_file(b"only.bin", decoy_free)];

        let index = CorpusIndex::build(&head, &unlimited_budget()).expect("index builds");
        let mut pool = WorkPool::new(u64::MAX);
        let counts: HashMap<[u8; 32], u64> = HashMap::new();
        let eligible = [region(payload), region(payload)];
        let outputs = run_matching(&eligible, &head, &index, &options(), &mut pool, &counts);

        assert!(matches!(outputs.dispositions[0], Disposition::Exact { .. }));
        let Disposition::Fragments(result) = &outputs.dispositions[1] else {
            panic!("expected fragment fallback for the duplicate");
        };
        assert_eq!(result.conserved, 0);
        assert_eq!(result.residue, payload.len() as u64);
    }

    #[test]
    fn longer_regions_should_win_scarce_copies_before_shorter_ones() {
        // A long and a shorter region overlap on the single surviving copy;
        // priority order (longer first) lets the long region consume it and
        // the short one finds only consumed bytes afterwards.
        let long_bytes: &'static [u8] = b"LONGER-REGION-HEADDATA-TAILBLOCK-SECONDHALF";
        let short_bytes: &'static [u8] = b"EADDATA-TAILBLOCK-SECONDHALF";
        let host_bytes = b"guard-LONGER-REGION-HEADDATA-TAILBLOCK-SECONDHALF-guard";
        let head = vec![leak_file(b"host.bin", host_bytes.to_vec())];
        let index = CorpusIndex::build(&head, &unlimited_budget()).expect("index builds");
        let mut pool = WorkPool::new(u64::MAX);
        let counts = HashMap::new();
        let eligible = [region(long_bytes), region(short_bytes)];

        let outputs = run_matching(&eligible, &head, &index, &options(), &mut pool, &counts);

        assert!(matches!(outputs.dispositions[0], Disposition::Exact { .. }));
        let Disposition::Fragments(result) = &outputs.dispositions[1] else {
            panic!("the short region must defer to fragments");
        };
        assert_eq!(result.conserved, 0);
        assert_eq!(result.residue, short_bytes.len() as u64);
    }

    #[test]
    fn equal_length_ties_should_break_deterministically_by_head_side_start() {
        // One copy exists for only one of two equal-length regions; repeated
        // runs must produce identical outcomes.
        let first: &'static [u8] = b"TIE-BLOCK-FIRST-CONTENT-01";
        let second: &'static [u8] = b"TIE-BLOCK-SECONDCONTENT-02";
        let host_bytes = b"zzTIE-BLOCK-FIRST-CONTENT-01zz";
        let head = vec![leak_file(b"tie.bin", host_bytes.to_vec())];
        let index = CorpusIndex::build(&head, &unlimited_budget()).expect("index builds");
        let counts = HashMap::new();
        let eligible = [region(first), region(second)];

        let mut first_pool = WorkPool::new(u64::MAX);
        let one = run_matching(
            &eligible,
            &head,
            &index,
            &options(),
            &mut first_pool,
            &counts,
        );
        let mut second_pool = WorkPool::new(u64::MAX);
        let two = run_matching(
            &eligible,
            &head,
            &index,
            &options(),
            &mut second_pool,
            &counts,
        );

        assert_eq!(one.dispositions, two.dispositions);
        assert!(matches!(one.dispositions[0], Disposition::Exact { .. }));
        assert!(!matches!(one.dispositions[1], Disposition::Exact { .. }));
    }

    #[test]
    fn hot_windows_everywhere_should_classify_ambiguous_common() {
        // The corpus repeats a 16-byte window far past the hot limit, so the
        // removed region has no cold window at all.
        let head = vec![leak_file(b"bulk.bin", vec![b' '; 400])];
        let index = CorpusIndex::build(&head, &unlimited_budget()).expect("index builds");
        let mut pool = WorkPool::new(u64::MAX);
        let counts = HashMap::new();
        let region_bytes: &'static [u8] = Box::leak(vec![b' '; 40].into_boxed_slice());
        let eligible = [EligibleRegion {
            digest: sha(region_bytes),
            bytes: region_bytes,
        }];

        let outputs = run_matching(&eligible, &head, &index, &options(), &mut pool, &counts);

        assert_eq!(outputs.dispositions[0], Disposition::AmbiguousCommon);
    }

    #[test]
    fn length_boundaries_at_k_should_match_flush_against_eof() {
        let exactly_k: &'static [u8] = b"EXACTLY-SIXTEEN!";
        let k_plus_one: &'static [u8] = b"EXACTLY-SIXTEEN!+";
        let head = vec![
            leak_file(b"k.bin", exactly_k.to_vec()),
            leak_file(b"kp1.bin", k_plus_one.to_vec()),
        ];

        let outputs = run(&[region(exactly_k), region(k_plus_one)], &head);

        assert!(matches!(outputs.dispositions[0], Disposition::Exact { .. }));
        let Disposition::Exact { destination, .. } = outputs.dispositions[1] else {
            panic!("K+1 flush-EOF occurrence must match");
        };
        assert_eq!((destination.head_file, destination.end), (1, 17));
    }

    #[test]
    fn consumption_should_stay_disjoint_and_merged_under_adversarial_orders() {
        // Deterministic pseudo-random overlapping consumes must always merge
        // into sorted disjoint intervals whose union equals an independently
        // tracked occupancy mask.
        const SPAN: u64 = 128;
        let mut log = ConsumptionLog::default();
        let mut occupied = vec![false; SPAN as usize];
        let mut state = 0x2545_F491_4F6C_DD1D_u64;

        for _ in 0..200 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let start = (state >> 33) % (SPAN - 24);
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let length = ((state >> 33) % 24) + 1;
            log.consume_for_tests(start, start + length);
            occupied[start as usize..(start + length) as usize].fill(true);

            let intervals = log.intervals();
            for pair in intervals.windows(2) {
                assert!(pair[0].1 < pair[1].0, "intervals must stay disjoint");
            }
            let covered: u64 = intervals.iter().map(|(low, high)| high - low).sum();
            let expected = occupied.iter().filter(|slot| **slot).count() as u64;
            assert_eq!(covered, expected, "union drift after consume({start})");
        }
    }

    #[test]
    fn budget_exhaustion_should_cut_mid_region_into_fragments_plus_unanalyzed() {
        let left: &'static [u8] = b"LEFT-FRAGMENT-PAYLOAD";
        let right: &'static [u8] = b"RIGHTFRAGMENTTAILXX";
        let mut host = left.to_vec();
        host.extend_from_slice(b"?");
        host.extend_from_slice(right);
        let head = vec![leak_file(b"cut.bin", host)];
        let mut whole = Vec::new();
        whole.extend_from_slice(left);
        whole.extend_from_slice(b"??");
        whole.extend_from_slice(right);
        let whole_bytes: &'static [u8] = Box::leak(whole.into_boxed_slice());

        let index = CorpusIndex::build(&head, &unlimited_budget()).expect("index builds");
        let tiny = budget_with(u64::MAX, u64::MAX, 600, u64::MAX, u64::MAX, 64);
        let mut pool = WorkPool::new(tiny.matching_work_units());
        let counts = HashMap::new();
        let eligible = [EligibleRegion {
            digest: sha(whole_bytes),
            bytes: whole_bytes,
        }];

        let outputs = run_matching(&eligible, &head, &index, &options(), &mut pool, &counts);

        match &outputs.dispositions[0] {
            Disposition::NotProcessed | Disposition::AmbiguousCommon => {
                panic!("expected a partial fragment result")
            }
            Disposition::Exact { .. } => panic!("the exact pass cannot fit this region"),
            Disposition::Fragments(result) => {
                assert!(result.unanalyzed > 0 || result.residue > 0);
                assert_eq!(
                    result.conserved + result.residue + result.unanalyzed,
                    whole_bytes.len() as u64
                );
            }
        }
    }

    #[test]
    fn surplus_verified_copies_should_flag_multiplicity_even_for_one_removed_copy() {
        // §8.6's second disjunct: a single removed region with two free
        // surviving occurrences consumes one and counts the other as
        // verified surplus, so the record relied on multiplicity counting.
        let payload: &'static [u8] = b"SURPLUS-CHECK-PAYLOAD-0123456789";
        let head = vec![
            leak_file(b"copy-a.bin", payload.to_vec()),
            leak_file(b"copy-b.bin", payload.to_vec()),
        ];
        let index = CorpusIndex::build(&head, &unlimited_budget()).expect("index builds");
        let mut pool = WorkPool::new(u64::MAX);
        let counts: HashMap<[u8; 32], u64> = HashMap::new();
        let eligible = [region(payload)];

        let outputs = run_matching(&eligible, &head, &index, &options(), &mut pool, &counts);

        let Disposition::Exact {
            destination,
            pairing_ambiguous,
        } = outputs.dispositions[0]
        else {
            panic!("expected an exact match, got {:?}", outputs.dispositions[0]);
        };
        assert!(pairing_ambiguous);
        assert_eq!((destination.head_file, destination.start), (0, 0));
        // The surplus occurrence is counted, never consumed.
        assert_eq!(outputs.consumption[0].intervals(), &[(0, 32)]);
        assert!(outputs.consumption[1].intervals().is_empty());
    }

    #[test]
    fn fragments_should_never_extend_leftward_into_consumed_head_bytes() {
        // The head copy's first half is already consumed by an earlier
        // region. Fragment matching must accept only the unconsumed second
        // half: the mirrored leftward extension stops at the consumed
        // boundary even though those bytes would otherwise compare equal.
        let prefix: &'static [u8] = b"PREFIXPART-AAAAAAAAAA";
        let suffix: &'static [u8] = b"QSUFFIXPART-BBBBBBBBBB";
        let host_bytes = join(&[b"LEAD|", prefix, suffix, b"|TRAILMARKER!"]);
        let head = vec![leak_file(b"host.bin", host_bytes)];

        let mut whole = Vec::new();
        whole.extend_from_slice(prefix);
        whole.extend_from_slice(suffix);
        let whole_bytes: &'static [u8] = Box::leak(whole.into_boxed_slice());
        let target = EligibleRegion {
            digest: sha(whole_bytes),
            bytes: whole_bytes,
        };

        let index = CorpusIndex::build(&head, &unlimited_budget()).expect("index builds");
        let mut consumption = vec![ConsumptionLog::default()];
        consumption[0].consume_for_tests(5, 5 + prefix.len() as u64);
        let mut pool = WorkPool::new(u64::MAX);
        let counts = HashMap::new();

        let result = super::match_fragments(
            &target,
            &head,
            &index,
            16,
            &mut pool,
            &mut consumption,
            &counts,
        )
        .expect("walk completes without budget pressure");

        assert_eq!(result.conserved, suffix.len() as u64);
        assert_eq!(result.residue, prefix.len() as u64);
        assert_eq!(result.destinations.len(), 1);
        assert_eq!(result.destinations[0].start, 5 + prefix.len() as u64);
        // The fragment's interval merges adjacently with the earlier one.
        assert_eq!(consumption[0].intervals(), &[(5, 48)]);
    }
}
