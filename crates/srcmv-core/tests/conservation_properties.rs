//! Property coverage and the labeled hard-packing fixture for the
//! conservation engine (plan §15).
//!
//! Directions, split exactly as pinned by the plan: soundness (a pass is
//! rechecked from the outcome alone), scatter completeness on friendly
//! packings, modified-pair scatter through unique-anchor gaps, duplicated
//! chunks exercising multiplicity counting, mutation detection (a corrupted
//! run must fail; a single corrupted byte passes only below the floor or,
//! asserted from the inputs, when provably absorbed; a provably absorbed
//! corruption passes), determinism, and budget conversion. The canonical
//! intersecting-regions fixture pins the documented `fail` outcome
//! byte-exactly per the `tests/fixtures/conservation/hard-packing` README.

use std::collections::HashMap;

use proptest::prelude::*;

use srcmv_core::{
    AuditFile, ConservationBudget, ConservationInput, ConservationOptions, ConservationOutcome,
    CoreError, InconclusiveReason, RegionStatus, Sha256Digest, Verdict, audit_conservation,
};

/// Deterministic stand-in digests: classification only ever compares digests
/// for equality, so a strong local mix suffices here and the engine's byte
/// verification does the real soundness work.
fn fabricated_digest(bytes: &[u8]) -> Sha256Digest {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ bytes.len() as u64;
    for chunk in bytes.chunks(8) {
        let mut word = [0_u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        state ^= u64::from_le_bytes(word);
        state = state
            .wrapping_mul(0xFF51_AFD7_ED55_8CCD)
            .rotate_left(29)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
    }
    let mut digest = [0_u8; 32];
    for slot in digest.chunks_mut(8) {
        state ^= state >> 31;
        state = state.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
        state ^= state >> 26;
        let bytes = state.to_le_bytes();
        let count = slot.len();
        slot.copy_from_slice(&bytes[..count]);
    }
    Sha256Digest(digest)
}

#[derive(Clone)]
struct Tree {
    files: Vec<(Vec<u8>, Vec<u8>, Sha256Digest)>,
}

impl Tree {
    fn new(files: &[(&[u8], &[u8])]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(path, bytes)| (path.to_vec(), bytes.to_vec(), fabricated_digest(bytes)))
                .collect(),
        }
    }

    fn view(&self) -> Vec<AuditFile<'_>> {
        self.files
            .iter()
            .map(|(path, bytes, digest)| AuditFile {
                path,
                bytes,
                digest,
            })
            .collect()
    }
}

fn audit_with(
    base: &Tree,
    head: &Tree,
    options: ConservationOptions,
    budget: ConservationBudget,
) -> Result<ConservationOutcome, CoreError> {
    audit_conservation(ConservationInput {
        base: &base.view(),
        head: &head.view(),
        options,
        budget,
    })
}

/// A friendly packing: concatenated chunks form one deleted base file and
/// permuted copies land at mutually disjoint offsets inside `files` created
/// head files, wrapped in padding noise.
#[derive(Clone)]
struct Packing {
    base: Tree,
    head: Tree,
    /// `(file index, start, end)` per planted chunk, in chunk order.
    placements: Vec<(usize, u64, u64)>,
}

const PAD_BEFORE: usize = 7;
const PAD_AFTER: usize = 9;
/// Two padding values above every friendly-alphabet byte.
const PAD_BYTES: [u8; 2] = [0xFE, 0xFD];

/// Builds deterministic chunks over pairwise-disjoint byte alphabets so
/// scattered copies cannot collide with padding or each other — the plan's
/// "friendly packings only" domain where greedy matching is complete.
fn friendly_chunks(seeds: &[u64]) -> Vec<Vec<u8>> {
    const ALPHABET: u64 = 100;
    let band_width = ALPHABET / seeds.len().max(1) as u64;
    let mut chunks = Vec::with_capacity(seeds.len());
    for (index, &seed) in seeds.iter().enumerate() {
        let base = index as u64 * band_width;
        let width = band_width.max(4);
        let length = 17 + (seed % 31) as usize;
        let mut chunk = Vec::with_capacity(length);
        for step in 0..length {
            let value =
                base + ((seed >> 8) ^ (step as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) % width;
            chunk.push(value as u8);
        }
        chunks.push(chunk);
    }
    chunks
}

/// Chunks for line-bracketed (anchor/gap) packings: the same disjoint-band
/// scheme restricted to printable non-terminator bytes, so a chunk byte can
/// never collide with the LF separators or blank-line placeholders the
/// bracketed extraction vocabulary is built from.
fn line_safe_chunks(seeds: &[u64]) -> Vec<Vec<u8>> {
    const ALPHABET: u64 = 94;
    let band_width = ALPHABET / seeds.len().max(1) as u64;
    let mut chunks = Vec::with_capacity(seeds.len());
    for (index, &seed) in seeds.iter().enumerate() {
        let base = index as u64 * band_width;
        let width = band_width.max(4).min(ALPHABET - base);
        let length = 17 + (seed % 31) as usize;
        let mut chunk = Vec::with_capacity(length);
        for step in 0..length {
            let value =
                base + ((seed >> 8) ^ (step as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) % width;
            chunk.push((0x21 + value) as u8);
        }
        chunks.push(chunk);
    }
    chunks
}

fn scatter(chunks: &[Vec<u8>], files: usize, pad_byte_a: u8, pad_byte_b: u8) -> Packing {
    let base_bytes: Vec<u8> = chunks.concat();
    let file_count = files.max(1);
    let mut heads: Vec<Vec<u8>> = vec![Vec::new(); file_count];
    let mut placements = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        let target = index % file_count;
        heads[target].extend(std::iter::repeat_n(
            pad_byte_a.wrapping_add(index as u8),
            PAD_BEFORE,
        ));
        let start = heads[target].len() as u64;
        heads[target].extend_from_slice(chunk);
        placements.push((target, start, start + chunk.len() as u64));
        heads[target].extend(std::iter::repeat_n(
            pad_byte_b.wrapping_add(index as u8),
            PAD_AFTER,
        ));
    }

    let base_path = b"data.bin".to_vec();
    let head_paths: Vec<Vec<u8>> = (0..file_count)
        .map(|index| format!("out-{index}.bin").into_bytes())
        .collect();
    Packing {
        base: Tree::new(&[]),
        head: Tree::new(&[]),
        placements,
    }
    .assemble(base_path, base_bytes, head_paths, heads)
}

impl Packing {
    fn assemble(
        self,
        base_path: Vec<u8>,
        base_bytes: Vec<u8>,
        head_paths: Vec<Vec<u8>>,
        heads: Vec<Vec<u8>>,
    ) -> Packing {
        let mut base = self.base;
        base.files = vec![(
            base_path.clone(),
            base_bytes.clone(),
            fabricated_digest(&base_bytes),
        )];
        let mut head = self.head;
        head.files = head_paths
            .into_iter()
            .zip(heads)
            .map(|(path, bytes)| {
                let digest = fabricated_digest(&bytes);
                (path, bytes, digest)
            })
            .collect();
        Packing {
            base,
            head,
            placements: self.placements,
        }
    }
}

/// Rechecks §9.2 plus the soundness core from the outcome and raw trees
/// alone: every destination must byte-match its source slice, stay in
/// bounds, and remain exclusively consumed across all records.
fn assert_soundness_and_identity(outcome: &ConservationOutcome, base: &Tree, head: &Tree) {
    let totals = outcome.totals;
    let terms = totals.conserved_exact_bytes
        + totals.conserved_fragments_bytes
        + totals.unmatched_substantial_bytes
        + totals.insubstantial_residue_bytes
        + totals.formatting_only_bytes
        + totals.ambiguous_common_bytes
        + totals.unanalyzed_bytes;
    assert_eq!(terms, totals.removed_bytes, "§9.2 ledger identity");
    assert_eq!(
        totals.conserved_bytes,
        totals.conserved_exact_bytes + totals.conserved_fragments_bytes
    );

    let base_by_path: HashMap<&[u8], &[u8]> = base
        .files
        .iter()
        .map(|(path, bytes, _)| (path.as_slice(), bytes.as_slice()))
        .collect();
    let head_by_path: HashMap<&[u8], &[u8]> = head
        .files
        .iter()
        .map(|(path, bytes, _)| (path.as_slice(), bytes.as_slice()))
        .collect();
    let mut occupied: HashMap<&[u8], Vec<(u64, u64)>> = HashMap::new();
    let mut verified_destination_bytes = 0_u64;

    for record in &outcome.records {
        let source = base_by_path
            .get(record.source_path.as_slice())
            .unwrap_or_else(|| panic!("unknown source path {:?}", record.source_path));
        let (start, end) = record.source_byte_range;
        assert_eq!(end - start, record.length);
        // Digest fields come from the engine's own SHA-256; the byte-level
        // destination checks below carry the real verification.
        let removed = &source[start as usize..end as usize];

        for destination in &record.destinations {
            let host = head_by_path
                .get(destination.path.as_slice())
                .unwrap_or_else(|| panic!("unknown destination {:?}", destination.path));
            let (low, high) = destination.byte_range;
            assert!(high > low, "destinations are nonempty");
            assert!(high <= host.len() as u64, "destination out of bounds");
            let destination_bytes = &host[low as usize..high as usize];
            assert!(
                removed
                    .windows(destination_bytes.len())
                    .any(|window| window == destination_bytes),
                "every destination must appear verbatim inside the removed slice"
            );
            verified_destination_bytes += high - low;

            let intervals = occupied.entry(destination.path.as_slice()).or_default();
            for &(other_low, other_high) in intervals.iter() {
                assert!(
                    other_high <= low || high <= other_low,
                    "consumed intervals must stay exclusive"
                );
            }
            intervals.push((low, high));
        }

        if record.status == RegionStatus::FormattingOnly {
            assert!(
                record.paired_context.is_some(),
                "formatting records annotate their counterpart"
            );
        }
    }

    assert_eq!(
        verified_destination_bytes, totals.conserved_bytes,
        "recorded destinations must account for every conserved byte"
    );
}

/// Fixed chunk length for run-corruption packings: large enough to host a
/// wound of `min_substantial_bytes ..= chunk_len - min_fragment_bytes` bytes
/// while keeping `min_fragment_bytes` pristine bytes on one side.
const MUT_CHUNK_LEN: usize = 64;
/// Default fragment floor (`--min-fragment-bytes`), also the pristine margin
/// kept before every planted wound.
const MIN_FRAGMENT_DEFAULT: u64 = 16;
/// Default substantiality floor (`--min-substantial-bytes`): the smallest
/// wound length that must surface as unmatched-substantial.
const WOUND_MIN: u64 = 32;
/// Largest wound leaving the chunk long enough to stay probeable:
/// `MUT_CHUNK_LEN - min_fragment_bytes`.
const WOUND_MAX: u64 = (MUT_CHUNK_LEN - MIN_FRAGMENT_DEFAULT as usize) as u64;

/// Wound filler from a fresh alphabet (0xB0..0xBF): disjoint from every
/// friendly-chunk band (< 100) and from all padding bytes, so wounded bytes
/// cannot appear anywhere else in a packing by construction.
fn fresh_alphabet_bytes(length: usize) -> Vec<u8> {
    (0..length).map(|step| 0xB0 + (step % 16) as u8).collect()
}

/// True iff `needle` occurs verbatim in some head file at a position that
/// does not intersect the corrupted copy `(file, start, end)`.
fn occurs_outside_corrupted_copy(
    head_files: &[(Vec<u8>, Vec<u8>, Sha256Digest)],
    corrupted_copy: (usize, u64, u64),
    needle: &[u8],
) -> bool {
    let (file, start, end) = corrupted_copy;
    head_files.iter().enumerate().any(|(rank, (_, bytes, _))| {
        if bytes.len() < needle.len() {
            return false;
        }
        (0..=bytes.len() - needle.len()).any(|pos| {
            let outside =
                rank != file || (pos as u64) + needle.len() as u64 <= start || pos as u64 >= end;
            outside && &bytes[pos..pos + needle.len()] == needle
        })
    })
}

/// §15's "provably absorbed", decided from the generated inputs alone: the
/// pristine removed payload still holds a probeable window (`K =
/// MIN_FRAGMENT_DEFAULT` bytes — the matcher's minimum verified context,
/// equal to the index window) that spans the wound offset and occurs
/// verbatim somewhere in the head tree outside the corrupted copy. Every
/// wounded byte is then carried by that escape occurrence, so a pass needs
/// no below-the-floor residue. This covers both duplicated-chunk escapes
/// and narrower coincidences such as a chunk-boundary suffix landing inside
/// an inter-copy padding run beside another chunk's copy.
fn wound_is_absorbed(
    head_files: &[(Vec<u8>, Vec<u8>, Sha256Digest)],
    corrupted_copy: (usize, u64, u64),
    pristine_region: &[u8],
    wound_offset: u64,
) -> bool {
    let window = MIN_FRAGMENT_DEFAULT as usize;
    let wound = wound_offset as usize;
    assert!(
        wound < pristine_region.len(),
        "wound must sit in the region"
    );
    let first_low = (wound + 1).saturating_sub(window);
    let last_low = wound.min(pristine_region.len().saturating_sub(window));
    (first_low..=last_low).any(|low| {
        occurs_outside_corrupted_copy(
            head_files,
            corrupted_copy,
            &pristine_region[low..low + window],
        )
    })
}

/// Builds one randomized-shape single-byte corruption case through the same
/// generator the property drives: friendly chunks padded to 34 bytes,
/// scattered into two head files, one byte of the victim copy flipped.
/// Returns the packing, the audit outcome, and whether the wound was
/// provably absorbed as computed from the inputs alone.
fn run_single_byte_corruption_case(
    seeds: &[u64],
    victim_choice: usize,
    position_choice: usize,
    corrupt_byte: u8,
) -> (Packing, ConservationOutcome, bool) {
    let mut distinct = friendly_chunks(seeds);
    // Keep every chunk comfortably above the substantiality floor.
    for chunk in &mut distinct {
        while chunk.len() < 34 {
            chunk.push(0x42);
        }
    }
    let victim = victim_choice % distinct.len();

    let mut packing = scatter(&distinct, 2, b'#', b'%');
    let (file, start, end) = packing.placements[victim];
    let span = end - start;
    assert!(span > 0, "planted copies are never empty");
    let offset = start + (position_choice as u64 % span);
    {
        let host = &mut packing.head.files[file].1;
        let original = host[offset as usize];
        let replacement = if corrupt_byte == original {
            original ^ 0x55
        } else {
            corrupt_byte
        };
        host[offset as usize] = replacement;
    }

    // Absorption is decided from the inputs ("assert which"): the pristine
    // payload is the concatenated chunk layout of the base file, and the
    // wound sits `offset - start` bytes into the victim chunk.
    let pristine_region: Vec<u8> = distinct.concat();
    let region_wound_offset: u64 = distinct[..victim]
        .iter()
        .map(|chunk| chunk.len() as u64)
        .sum::<u64>()
        + (offset - start);
    let absorbed = wound_is_absorbed(
        &packing.head.files,
        packing.placements[victim],
        &pristine_region,
        region_wound_offset,
    );

    let outcome = audit_with(
        &packing.base,
        &packing.head,
        ConservationOptions::default(),
        ConservationBudget::default(),
    )
    .expect("corrupted audits must complete");
    (packing, outcome, absorbed)
}

// Failure persistence is pinned to the committed regressions file below so
// minimized seeds land in version control regardless of the default
// SourceParallel resolution (which cannot find a crate-root lib.rs under the
// standard src/ layout).
proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(
            "proptest-regressions/tests/conservation_properties.txt",
        ))),
        ..ProptestConfig::with_cases(128)
    })]

    #[test]
    fn scatter_packings_should_pass_with_complete_conservation(
        seeds in prop::collection::vec(any::<u64>(), 1..=5),
        files in 1_usize..=3,
    ) {
        let distinct = friendly_chunks(&seeds);

        let packing = scatter(
            &distinct,
            files,
            PAD_BYTES[0],
            PAD_BYTES[1],
        );
        let outcome = audit_with(
            &packing.base,
            &packing.head,
            ConservationOptions::default(),
            ConservationBudget::default(),
        )
        .expect("friendly packings must complete");

        prop_assert_eq!(outcome.verdict, Verdict::Pass, "unexpected non-pass");
        prop_assert_eq!(outcome.totals.removed_regions, 1);
        prop_assert_eq!(outcome.totals.removed_bytes, outcome.totals.conserved_bytes);
        prop_assert_eq!(outcome.totals.unmatched_substantial_bytes, 0);
        prop_assert!(outcome.reasons.is_empty());
        prop_assert_eq!(outcome.records.len(), 1);
        prop_assert_eq!(outcome.records[0].status, RegionStatus::Conserved);

        // Every planted copy must sit inside some accepted fragment
        // destination. Neighboring copies whose padding happens to match the
        // concatenated region bytes may merge into one longer destination,
        // which is still exact, exclusive conservation.
        let record = &outcome.records[0];
        prop_assert!(!record.destinations.is_empty());
        let path_to_index: HashMap<Vec<u8>, usize> = packing
            .head
            .files
            .iter()
            .enumerate()
            .map(|(index, (path, _, _))| (path.clone(), index))
            .collect();
        for &(file, start, end) in &packing.placements {
            let covered = record.destinations.iter().any(|destination| {
                path_to_index.get(&destination.path) == Some(&file)
                    && destination.byte_range.0 <= start
                    && end <= destination.byte_range.1
            });
            prop_assert!(covered, "planted copy {start}..{end} must be covered");
        }

        let placed: u64 = distinct.iter().map(|chunk| chunk.len() as u64).sum();
        let total_head: u64 = packing
            .head
            .files
            .iter()
            .map(|(_, bytes, _)| bytes.len() as u64)
            .sum();
        prop_assert_eq!(outcome.totals.new_bytes, total_head - placed);

        assert_soundness_and_identity(&outcome, &packing.base, &packing.head);
    }

    #[test]
    fn corrupted_run_inside_one_chunk_must_fail_with_unmatched_wound_coverage(
        seeds in prop::collection::vec(any::<u64>(), 2..=3),
        victim_choice in 0_usize..4,
        wound_length in WOUND_MIN..=WOUND_MAX,
    ) {
        let mut chunks = friendly_chunks(&seeds);
        for chunk in &mut chunks {
            // Normalize every chunk to MUT_CHUNK_LEN by repeating its own
            // in-band bytes so cross-chunk alphabet disjointness survives.
            let original = chunk.len();
            while chunk.len() < MUT_CHUNK_LEN {
                chunk.push(chunk[chunk.len() % original]);
            }
            assert_eq!(chunk.len(), MUT_CHUNK_LEN);
        }
        let victim = victim_choice % chunks.len();

        let mut packing = scatter(&chunks, 2, PAD_BYTES[0], PAD_BYTES[1]);
        let (file, start, end) = packing.placements[victim];
        prop_assert_eq!(end - start, MUT_CHUNK_LEN as u64);
        // Keep min_fragment_bytes of pristine bytes before the wound so the
        // surrounding context still matches fragmentarily; the run length is
        // drawn from min_substantial_bytes ..= chunk_len - min_fragment_bytes
        // so the wound itself always clears the substantiality floor.
        let wound_start = start + MIN_FRAGMENT_DEFAULT;
        let wound_len = wound_length as usize;
        let host = &mut packing.head.files[file].1;
        let range = wound_start as usize..wound_start as usize + wound_len;
        let replacement = fresh_alphabet_bytes(wound_len);
        for (slot, &byte) in range.clone().zip(replacement.iter()) {
            host[slot] = byte;
        }

        let outcome = audit_with(
            &packing.base,
            &packing.head,
            ConservationOptions::default(),
            ConservationBudget::default(),
        )
        .expect("corrupted audits must complete");

        // The wound bytes come from a fresh alphabet absent everywhere else
        // in the head tree, so absorption elsewhere is impossible by
        // construction and the expected outcome is exactly Fail.
        prop_assert_eq!(outcome.verdict, Verdict::Fail, "wounded copy must fail");
        prop_assert!(outcome.reasons.is_empty());
        prop_assert!(
            outcome.totals.unmatched_substantial_bytes >= wound_length,
            "unmatched total must cover the {wound_length}-byte wound"
        );

        // The unmatched record's source range must contain the corrupted
        // offsets mapped back onto the base-side region layout.
        let base_wound_start = victim as u64 * MUT_CHUNK_LEN as u64 + MIN_FRAGMENT_DEFAULT;
        let base_wound_end = base_wound_start + wound_length;
        let unmatched: Vec<_> = outcome
            .records
            .iter()
            .filter(|record| record.status == RegionStatus::UnmatchedSubstantial)
            .collect();
        prop_assert_eq!(unmatched.len(), 1);
        let (low, high) = unmatched[0].source_byte_range;
        prop_assert!(
            low <= base_wound_start && high >= base_wound_end,
            "unmatched record {low}..{high} must cover wound {base_wound_start}..{base_wound_end}"
        );

        assert_soundness_and_identity(&outcome, &packing.base, &packing.head);
    }

    #[test]
    fn single_byte_corruption_passes_only_because_residue_stays_below_the_floor(
        seeds in prop::collection::vec(any::<u64>(), 2..=3),
        victim_choice in 0_usize..4,
        position_choice in 0_usize..64,
        corrupt_byte in any::<u8>(),
    ) {
        let (packing, outcome, absorbed) =
            run_single_byte_corruption_case(&seeds, victim_choice, position_choice, corrupt_byte);

        // Honest relabel per §15's mutation bullet ("never `pass` with
        // `unmatched_substantial_bytes == 0` unless the mutation was
        // provably absorbed — assert which"). Derived from what this
        // generator constructs — one coarse region over the whole base
        // file, a one-byte wound strictly inside a planted copy, tiny
        // inputs under default budgets — the only reachable verdict is
        // `pass` with nothing counted unmatched-substantial (the wound is
        // far below the 32-byte substantiality floor, and the corrupted
        // cell blocks every in-copy walk across it). The two admissible
        // ledger shapes are:
        // - absorbed: some probeable window of the pristine payload still
        //   occurs verbatim outside the corrupted copy (escape copy), so
        //   the surviving occurrence takes the credit and the insubstantial
        //   residue MAY be 0;
        // - not absorbed: the pass is legitimate only because the walked-
        //   but-unmatched residue around the one-byte wound stays below the
        //   floor, so at least one residue byte must exist.
        prop_assert_eq!(outcome.verdict, Verdict::Pass);
        prop_assert!(outcome.reasons.is_empty());
        prop_assert_eq!(outcome.totals.unmatched_substantial_bytes, 0);
        if absorbed {
            // Residue may legitimately be 0 here: the escape copy absorbs
            // the mutation.
        } else {
            prop_assert!(
                outcome.totals.insubstantial_residue_bytes >= 1,
                "unabsorbed single-byte wounds must leave visible residue"
            );
        }

        assert_soundness_and_identity(&outcome, &packing.base, &packing.head);
    }

    #[test]
    fn repeated_runs_should_be_identical(
        seeds in prop::collection::vec(any::<u64>(), 1..=4),
        seed in any::<u64>(),
    ) {
        let distinct = friendly_chunks(&seeds);

        let packing = scatter(
            &distinct,
            (seed % 3) as usize + 1,
            PAD_BYTES[0],
            PAD_BYTES[1],
        );
        let first = audit_with(
            &packing.base,
            &packing.head,
            ConservationOptions::default(),
            ConservationBudget::default(),
        )
        .expect("first run completes");
        let second = audit_with(
            &packing.base,
            &packing.head,
            ConservationOptions::default(),
            ConservationBudget::default(),
        )
        .expect("second run completes");
        prop_assert_eq!(first, second);
    }

    #[test]
    fn lowered_budgets_should_convert_to_clean_reasons_without_panics(
        seeds in prop::collection::vec(any::<u64>(), 1..=3),
        extraction_units in prop::sample::select(vec![0_u64, 60, 400, 5_000]),
        matching_units in prop::sample::select(vec![8_u64, 300, 40_000, u64::MAX]),
    ) {
        let distinct = friendly_chunks(&seeds);

        let packing = scatter(&distinct, 2, PAD_BYTES[0], PAD_BYTES[1]);
        let budget = ConservationBudget::new(
            extraction_units,
            u64::MAX,
            matching_units,
            u64::MAX,
            u64::MAX,
            64,
        );

        match audit_with(
            &packing.base,
            &packing.head,
            ConservationOptions::default(),
            budget,
        ) {
            Ok(outcome) => {
                let totals = outcome.totals;
                let terms = totals.conserved_exact_bytes
                    + totals.conserved_fragments_bytes
                    + totals.unmatched_substantial_bytes
                    + totals.insubstantial_residue_bytes
                    + totals.formatting_only_bytes
                    + totals.ambiguous_common_bytes
                    + totals.unanalyzed_bytes;
                prop_assert_eq!(terms, totals.removed_bytes, "§9.2 holds on partial runs");
                if totals.unanalyzed_bytes > 0 {
                    prop_assert!(
                        outcome.reasons.contains(&InconclusiveReason::WorkBudgetExceeded),
                        "truncated work must surface its reason"
                    );
                }
            }
            Err(CoreError::ResourceLimitExceeded { .. }) => {
                // Early envelope errors are sanctioned for failures that hit
                // before any region matched (§9.3 mechanical boundary).
            }
            Err(error) => prop_assert!(false, "unexpected error: {error:?}"),
        }
    }

    #[test]
    fn modified_pair_scatter_through_anchor_gaps_should_pass_completely(
        seeds in prop::collection::vec(any::<u64>(), 1..=4),
        rotation in 0_usize..8,
        files in 1_usize..=3,
    ) {
        // Base file split by unique anchor lines around scattered chunks; the
        // head keeps the anchors with a blank line where each chunk stood,
        // and permuted copies scatter into other head files. Packings stay
        // non-overlapping, the friendly domain where greedy matching is
        // complete, so every gap trims to exactly its chunk bytes.
        let chunks = line_safe_chunks(&seeds);
        let count = chunks.len();

        let mut base_bytes = Vec::new();
        let mut anchors_with_blanks = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let anchor_body = format!("unique-anchor-{index:02}-marker").into_bytes();
            prop_assume!(
                !chunk
                    .windows(anchor_body.len())
                    .any(|window| window == anchor_body.as_slice())
            );
            base_bytes.extend_from_slice(&anchor_body);
            base_bytes.push(b'\n');
            base_bytes.extend_from_slice(chunk);
            base_bytes.push(b'\n');
            anchors_with_blanks.extend_from_slice(&anchor_body);
            anchors_with_blanks.extend_from_slice(b"\n\n");
        }

        let file_count = files.max(1);
        let mut heads: Vec<Vec<u8>> = vec![Vec::new(); file_count];
        let mut placements = Vec::with_capacity(count);
        for slot in 0..count {
            let index = (slot + rotation) % count;
            let target = slot % file_count;
            heads[target].extend(std::iter::repeat_n(
                PAD_BYTES[0].wrapping_add(slot as u8),
                PAD_BEFORE,
            ));
            let start = heads[target].len() as u64;
            heads[target].extend_from_slice(&chunks[index]);
            // +1 skips the modified head file that leads the head layout.
            placements.push((target + 1, start, start + chunks[index].len() as u64));
            heads[target].extend(std::iter::repeat_n(
                PAD_BYTES[1].wrapping_add(slot as u8),
                PAD_AFTER,
            ));
        }

        let mut head_paths: Vec<Vec<u8>> = vec![b"mod/data.txt".to_vec()];
        head_paths.extend((0..file_count).map(|index| format!("out-{index}.bin").into_bytes()));
        let mut all_heads = vec![anchors_with_blanks];
        all_heads.extend(heads);
        let placed: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();
        // Every blank-line placeholder context trims to empty, so the only
        // new bytes are the padding around the created out-file copies.
        let created_total: u64 = all_heads.iter().skip(1).map(|bytes| bytes.len() as u64).sum();
        let packing = Packing {
            base: Tree::new(&[]),
            head: Tree::new(&[]),
            placements,
        }
        .assemble(b"mod/data.txt".to_vec(), base_bytes, head_paths, all_heads);

        let outcome = audit_with(
            &packing.base,
            &packing.head,
            ConservationOptions::default(),
            ConservationBudget::default(),
        )
        .expect("friendly packings must complete");

        prop_assert_eq!(outcome.verdict, Verdict::Pass);
        prop_assert!(outcome.reasons.is_empty());
        prop_assert_eq!(outcome.totals.removed_regions, count as u64);
        prop_assert_eq!(outcome.totals.conserved_bytes, placed);
        prop_assert_eq!(outcome.totals.unmatched_substantial_bytes, 0);
        prop_assert_eq!(outcome.totals.insubstantial_residue_bytes, 0);
        prop_assert_eq!(outcome.totals.formatting_only_bytes, 0);
        prop_assert_eq!(outcome.totals.unanalyzed_bytes, 0);
        prop_assert_eq!(outcome.totals.pairing_ambiguous_regions, 0);

        prop_assert_eq!(outcome.totals.new_bytes, created_total - placed);

        let path_to_index: HashMap<Vec<u8>, usize> = packing
            .head
            .files
            .iter()
            .enumerate()
            .map(|(index, (path, _, _))| (path.clone(), index))
            .collect();
        for record in &outcome.records {
            prop_assert!(!record.destinations.is_empty());
        }
        for &(file, start, end) in &packing.placements {
            let covered = outcome.records.iter().any(|record| {
                record.destinations.iter().any(|destination| {
                    path_to_index.get(&destination.path) == Some(&file)
                        && destination.byte_range.0 <= start
                        && end <= destination.byte_range.1
                })
            });
            prop_assert!(covered, "planted copy {start}..{end} must be covered");
        }

        assert_soundness_and_identity(&outcome, &packing.base, &packing.head);
    }

    #[test]
    fn duplicated_chunks_planted_twice_should_pass_with_pairing_flags(
        seeds in prop::collection::vec(any::<u64>(), 2..=4),
        rotation in 0_usize..4,
    ) {
        // One chunk planted twice in the base with two distinct survivors in
        // the head exercises multiplicity counting randomly: both duplicate
        // instances must take their credit under one-to-one consumption and
        // carry the pairing flag, while single-copy regions stay unflagged.
        let mut chunks = line_safe_chunks(&seeds);
        let duplicated = chunks[chunks.len() - 1].clone();
        chunks.push(duplicated);

        let mut base_bytes = Vec::new();
        let mut anchors_with_blanks = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let anchor_body = format!("dup-anchor-{index:02}-marker").into_bytes();
            prop_assume!(
                !chunk
                    .windows(anchor_body.len())
                    .any(|window| window == anchor_body.as_slice())
            );
            base_bytes.extend_from_slice(&anchor_body);
            base_bytes.push(b'\n');
            base_bytes.extend_from_slice(chunk);
            base_bytes.push(b'\n');
            anchors_with_blanks.extend_from_slice(&anchor_body);
            anchors_with_blanks.extend_from_slice(b"\n\n");
        }

        // One survivor per instance plus nothing shared: every planted copy
        // sits at a disjoint offset, so greedy completeness holds.
        let file_count = 2_usize;
        let mut heads: Vec<Vec<u8>> = vec![Vec::new(); file_count];
        let mut placements = Vec::with_capacity(chunks.len());
        for (slot, chunk) in chunks.iter().enumerate() {
            let target = (slot + rotation) % file_count;
            heads[target].extend(std::iter::repeat_n(
                PAD_BYTES[0].wrapping_add(slot as u8),
                PAD_BEFORE,
            ));
            let start = heads[target].len() as u64;
            heads[target].extend_from_slice(chunk);
            // +1 skips the modified head file that leads the head layout.
            placements.push((target + 1, start, start + chunk.len() as u64));
            heads[target].extend(std::iter::repeat_n(
                PAD_BYTES[1].wrapping_add(slot as u8),
                PAD_AFTER,
            ));
        }

        let mut head_paths: Vec<Vec<u8>> = vec![b"mod/data.txt".to_vec()];
        head_paths.extend((0..file_count).map(|index| format!("out-{index}.bin").into_bytes()));
        let mut all_heads = vec![anchors_with_blanks];
        all_heads.extend(heads);
        let placed: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();
        let created_total: u64 = all_heads.iter().skip(1).map(|bytes| bytes.len() as u64).sum();
        let packing = Packing {
            base: Tree::new(&[]),
            head: Tree::new(&[]),
            placements,
        }
        .assemble(b"mod/data.txt".to_vec(), base_bytes, head_paths, all_heads);

        let outcome = audit_with(
            &packing.base,
            &packing.head,
            ConservationOptions::default(),
            ConservationBudget::default(),
        )
        .expect("duplicated-chunk packings must complete");

        prop_assert_eq!(outcome.verdict, Verdict::Pass);
        prop_assert!(outcome.reasons.is_empty());
        prop_assert_eq!(outcome.totals.removed_regions, chunks.len() as u64);
        prop_assert_eq!(
            outcome.totals.conserved_bytes,
            placed,
            "every instance consumes its own survivor"
        );
        prop_assert_eq!(outcome.totals.unmatched_substantial_bytes, 0);
        prop_assert_eq!(outcome.totals.insubstantial_residue_bytes, 0);
        prop_assert_eq!(outcome.totals.formatting_only_bytes, 0);
        prop_assert_eq!(outcome.totals.new_bytes, created_total - placed);
        prop_assert!(
            outcome.totals.pairing_ambiguous_regions >= 1,
            "multiplicity counting must flag the duplicated pair"
        );
        prop_assert_eq!(outcome.totals.pairing_ambiguous_regions, 2);

        assert_soundness_and_identity(&outcome, &packing.base, &packing.head);
    }
}

/// Labeled fixture: the canonical intersecting-regions packing from
/// `tests/fixtures/conservation/hard-packing`, rebuilt in-test from its
/// pinned byte recipe. Distinct region contents compete for intersecting
/// occurrences; exclusive accounting makes the interior segment of `U`
/// genuinely unconserved, so the correct outcome is a deterministic `fail`.
#[test]
fn conservation_audit_should_pin_hard_packing_fixture_outcome() {
    // U: ascending printable ASCII without 'y', then the UTF-8 ellipsis
    // bytes E2 80 A6 — 96 pairwise-distinct terminator-free bytes.
    let mut u: Vec<u8> = (0x21..=0x7E).filter(|byte| *byte != b'y').collect();
    assert_eq!(u.len(), 93);
    u.extend_from_slice(&[0xE2, 0x80, 0xA6]);
    assert_eq!(u.len(), 96);

    let r1 = u.clone();
    let mut r2 = u[79..96].to_vec();
    r2.extend(std::iter::repeat_n(b'y', 15));
    assert_eq!(r2.len(), 32);

    let base_f1 = [
        b"P\n".as_slice(),
        r1.as_slice(),
        b"\nQ\n".as_slice(),
        r2.as_slice(),
        b"\nR\n".as_slice(),
    ]
    .concat();
    let head_f1: &[u8] = b"P\n\nQ\n\nR\n";
    let head_a = [
        &u[..],
        &std::iter::repeat_n(b'y', 15).collect::<Vec<u8>>()[..],
    ]
    .concat();
    assert_eq!(head_a.len(), 111);

    let base = Tree::new(&[(b"f1.txt", &base_f1)]);
    let head = Tree::new(&[(b"f1.txt", head_f1), (b"a.txt", &head_a)]);

    let outcome = audit_with(
        &base,
        &head,
        ConservationOptions::default(),
        ConservationBudget::default(),
    )
    .expect("the pinned fixture must complete");

    assert_eq!(outcome.verdict, Verdict::Fail);
    assert!(outcome.reasons.is_empty());
    let totals = outcome.totals;
    assert_eq!(totals.removed_regions, 2);
    assert_eq!(totals.removed_bytes, 128);
    assert_eq!(totals.conserved_exact_bytes, 96);
    assert_eq!(totals.conserved_fragments_bytes, 0);
    assert_eq!(totals.unmatched_substantial_bytes, 32);
    assert_eq!(totals.new_bytes, 15);
    assert_eq!(totals.pairing_ambiguous_regions, 0);

    assert_eq!(outcome.records.len(), 2);
    let conserved = &outcome.records[0];
    assert_eq!(conserved.status, RegionStatus::Conserved);
    assert_eq!(conserved.length, 96);
    assert_eq!(conserved.destinations.len(), 1);
    assert_eq!(conserved.destinations[0].path, b"a.txt".to_vec());
    assert_eq!(conserved.destinations[0].byte_range, (0, 96));

    let unmatched = &outcome.records[1];
    assert_eq!(unmatched.status, RegionStatus::UnmatchedSubstantial);
    assert_eq!(unmatched.length, 32);
    assert!(unmatched.destinations.is_empty());

    assert_soundness_and_identity(&outcome, &base, &head);
}

/// Deterministic ABSORBED case pinning §15/AC#3's escape hatch: a corrupted
/// run whose ORIGINAL bytes appear verbatim elsewhere in the head tree.
/// The audit passes because the removed payload is provably present —
/// byte-verified and exclusively consumed — at the pristine destination;
/// the wounded copy simply holds no claim on those bytes anymore. This
/// asserts WHICH occurrence absorbs the mutation instead of hand-waving
/// the pass.
#[test]
fn conservation_audit_should_pin_absorbed_corruption_as_pass() {
    // U: the canonical distinct terminator-free payload recipe.
    let mut payload: Vec<u8> = (0x21..=0x7E).filter(|byte| *byte != b'y').collect();
    payload.extend_from_slice(&[0xE2, 0x80, 0xA6]);
    assert_eq!(payload.len(), 96);

    // Wound [16..48) with fresh-alphabet bytes (0xB0..0xBF, absent from the
    // payload alphabet).
    let mut wounded = payload.clone();
    for (step, slot) in (16..48).enumerate() {
        wounded[slot] = 0xB0 + (step % 16) as u8;
    }
    assert_ne!(wounded, payload);

    let base = Tree::new(&[(b"gone/payload.bin", &payload)]);
    let head = Tree::new(&[
        (b"out/pristine.bin", &payload),
        (b"out/wounded.bin", &wounded),
    ]);

    let outcome = audit_with(
        &base,
        &head,
        ConservationOptions::default(),
        ConservationBudget::default(),
    )
    .expect("absorbed-corruption audits must complete");

    assert_eq!(outcome.verdict, Verdict::Pass);
    assert!(outcome.reasons.is_empty());
    assert_eq!(outcome.totals.conserved_exact_bytes, payload.len() as u64);
    assert_eq!(outcome.totals.unmatched_substantial_bytes, 0);
    assert_eq!(outcome.totals.pairing_ambiguous_regions, 0);

    // The surviving copy carries the credit, and it does so VERBATIM: this
    // assertion is the "provably absorbed (assert which)" demonstration.
    assert_eq!(outcome.records.len(), 1);
    let record = &outcome.records[0];
    assert_eq!(record.status, RegionStatus::Conserved);
    assert_eq!(record.destinations.len(), 1);
    let destination = &record.destinations[0];
    assert_eq!(destination.path, b"out/pristine.bin".to_vec());
    assert_eq!(
        destination.byte_range,
        (0, payload.len() as u64),
        "the pristine copy absorbs the entire removed payload"
    );
    let host = head
        .files
        .iter()
        .find(|(path, _, _)| path.as_slice() == b"out/pristine.bin")
        .expect("pristine host present");
    let (low, high) = destination.byte_range;
    assert_eq!(
        &host.1[low as usize..high as usize],
        payload.as_slice(),
        "consumed destination bytes equal the removed payload exactly"
    );

    assert_soundness_and_identity(&outcome, &base, &head);
}

/// Committed replay of the randomized counterexample reported for
/// `single_byte_corruption_passes_only_because_residue_stays_below_the_floor`:
/// proptest-minimized seed `[0, 8739041743340106121, 0]`, victim_choice=1,
/// position_choice=43, corrupt_byte=0. The pristine base byte 37 equals the
/// `b'%'` inter-copy padding byte, so the region suffix `victim tail ++
/// chunk 2` occurs verbatim inside out-0.bin's padding run beside chunk 2's
/// copy: the wound is provably absorbed (§15), so the audit must pass with
/// ZERO insubstantial residue — the shape that used to trip the old
/// unconditional `residue >= 1` assertion.
#[test]
fn single_byte_absorbed_wound_counterexample_replays_as_pass_without_residue() {
    let (packing, outcome, absorbed) =
        run_single_byte_corruption_case(&[0, 8739041743340106121, 0], 1, 43, 0);

    assert!(absorbed, "this shape must classify as provably absorbed");
    assert_eq!(outcome.verdict, Verdict::Pass);
    assert!(outcome.reasons.is_empty());
    assert_eq!(outcome.totals.unmatched_substantial_bytes, 0);
    assert_eq!(
        outcome.totals.insubstantial_residue_bytes, 0,
        "absorbed wounds legitimately leave no walked residue"
    );

    assert_soundness_and_identity(&outcome, &packing.base, &packing.head);
}
