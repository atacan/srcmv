//! Phase 5 fuzz-regression replay for the conservation engine (plan §15).
//!
//! Replays every `conservation_matcher` seed from the repository-wide manifest
//! `tests/fuzz-regressions/seeds.json` through the public
//! [`audit_conservation`](srcmv_core::audit_conservation) engine, asserting no
//! panic plus the §9.2 ledger identity for every completed run. This is the
//! stable-toolchain consumer of the seeds that nightly `cargo-fuzz`
//! (`crates/srcmv-core/fuzz/`) may append after minimizing a defect; both
//! harnesses synthesize the identical two-tree projection, so a minimized
//! artifact replays identically in either one.
//!
//! The consumer lives beside the protocol layer rather than in `srcmv-core`
//! tests because the pure domain crate deliberately keeps its dev-dependencies
//! minimal (proptest only) and has no JSON parser; this crate already owns the
//! strict-JSON tooling and reads the same manifest for the Phase 9 artifact
//! invariant test in `hardening.rs`.
//!
//! Two-tree construction per seed (documented in
//! `tests/fuzz-regressions/README.md`, mirroring the cargo-fuzz harness):
//! base tree holds `base/changed.txt` = `"frame\n" ++ region ++ "\ntrailer"`
//! plus deleted twin `base/vanished.bin` = region bytes; head tree holds the
//! created corpus file `head/corpus.bin`. Both base paths are absent from the
//! head tree, so extraction always yields whole-file removed regions and the
//! matcher probes the corpus index with arbitrary attacker-shaped bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use proptest::prelude::*;

use srcmv_core::{
    AuditFile, ConservationBudget, ConservationInput, ConservationOptions, CoreError, Sha256Digest,
    Verdict, audit_conservation,
};

/// One decoded `conservation_matcher` seed: the `(region, corpus)` pair plus
/// the §6.1 option bits.
#[derive(Clone, Debug)]
struct SeedCase {
    region: Vec<u8>,
    corpus: Vec<u8>,
    strict_uniqueness: bool,
    count_formatting: bool,
}

/// Parsed once per process so the campaign adds no repeated manifest I/O
/// across generated cases.
fn seed_cases() -> &'static [SeedCase] {
    static CASES: OnceLock<Vec<SeedCase>> = OnceLock::new();
    CASES.get_or_init(|| {
        let cases = load_seed_cases();
        assert!(
            !cases.is_empty(),
            "the committed manifest must carry at least one conservation_matcher seed"
        );
        cases
    })
}

/// Loads and decodes every `conservation_matcher` seed from the committed
/// manifest. A malformed committed seed fails loudly: this consumer is the
/// guard that keeps the manifest machine-checkable.
fn load_seed_cases() -> Vec<SeedCase> {
    let manifest_path = repository_root().join("tests/fuzz-regressions/seeds.json");
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(manifest_path).expect("fuzz seed manifest should be readable"),
    )
    .expect("fuzz seed manifest should be JSON");
    let mut cases = Vec::new();
    for seed in manifest["seeds"]
        .as_array()
        .expect("seed list should be an array")
    {
        if seed["surface"].as_str() != Some("conservation_matcher") {
            continue;
        }
        let hex = seed["hex"].as_str().unwrap_or_default();
        let payload = decode_lower_hex(hex)
            .unwrap_or_else(|| panic!("conservation_matcher seed hex must be lowercase: {hex}"));
        let header = [
            payload.first().copied().unwrap_or_default(),
            payload.get(1).copied().unwrap_or_default(),
        ];
        let region_length = usize::from(u16::from_le_bytes(header));
        let rest = payload
            .get(2..)
            .unwrap_or_else(|| panic!("conservation_matcher seed lacks the u16-le length header"));
        assert!(
            region_length <= rest.len(),
            "conservation_matcher seed length header overruns its hex payload"
        );
        let mask = seed["options_mask"].as_u64().unwrap_or(0);
        cases.push(SeedCase {
            region: rest[..region_length].to_vec(),
            corpus: rest[region_length..].to_vec(),
            strict_uniqueness: mask & 1 != 0,
            count_formatting: mask & 2 != 0,
        });
    }
    cases
}

/// Decodes one concatenated lowercase-hex string back to raw bytes.
fn decode_lower_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

/// Deterministic stand-in digests, exactly as in
/// `crates/srcmv-core/tests/conservation_properties.rs`: classification only
/// ever compares digests for equality, while the engine verifies matched
/// bytes directly against the real slices.
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

/// One synthesized tree as owned `(path, bytes, digest)` rows.
type SynthesizedTree = Vec<(Vec<u8>, Vec<u8>, Sha256Digest)>;

/// Synthesizes the two-tree projection for one seed.
///
/// Base tree: frame file carrying the region bytes between fixed markers plus
/// a deleted twin holding the raw region. Head tree: the raw corpus as a
/// created file. Paths are chosen so no base path exists in the head tree,
/// making extraction independent of the arbitrary byte contents.
fn synthesize(case: &SeedCase) -> (SynthesizedTree, SynthesizedTree) {
    let mut changed = b"frame\n".to_vec();
    changed.extend_from_slice(&case.region);
    changed.extend_from_slice(b"\ntrailer");

    let base_files = vec![
        (
            b"base/changed.txt".to_vec(),
            changed.clone(),
            fabricated_digest(&changed),
        ),
        (
            b"base/vanished.bin".to_vec(),
            case.region.clone(),
            fabricated_digest(&case.region),
        ),
    ];
    let head_files = vec![(
        b"head/corpus.bin".to_vec(),
        case.corpus.clone(),
        fabricated_digest(&case.corpus),
    )];
    (base_files, head_files)
}

fn view(files: &SynthesizedTree) -> Vec<AuditFile<'_>> {
    files
        .iter()
        .map(|(path, bytes, digest)| AuditFile {
            path,
            bytes,
            digest,
        })
        .collect()
}

/// Runs one seed through the public engine and asserts the regression
/// contract: no panic, the §9.2 ledger identity on every completed run,
/// pass-verdict soundness gates per the options mask, and full determinism.
fn replay_seed(case: &SeedCase) {
    let (base_files, head_files) = synthesize(case);
    let options = ConservationOptions::new(32, 16, case.strict_uniqueness, case.count_formatting)
        .expect("seed-derived options satisfy the §6.1 floors");
    let input = ConservationInput {
        base: &view(&base_files),
        head: &view(&head_files),
        options,
        budget: ConservationBudget::default(),
    };

    let outcome = match audit_conservation(input) {
        Ok(outcome) => outcome,
        // Only the sanctioned pre-match resource-limit boundary may error
        // (§9.3); anything else is a regression.
        Err(CoreError::ResourceLimitExceeded { .. }) => return,
        Err(error) => panic!("unexpected engine error for {case:?}: {error:?}"),
    };

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

    if outcome.verdict == Verdict::Pass {
        assert_eq!(
            totals.unmatched_substantial_bytes, 0,
            "a pass leaves no substantial residue"
        );
        if case.count_formatting {
            assert_eq!(
                totals.formatting_only_bytes, 0,
                "count-formatting passes cannot carry formatting-only bytes"
            );
        }
    }

    // Determinism: identical inputs produce identical outcomes (plan §15).
    let replayed =
        audit_conservation(input).expect("second identical run must complete like the first");
    assert_eq!(outcome, replayed, "engine must stay deterministic");
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(
            "proptest-regressions/tests/conservation_fuzz_regressions.txt",
        ))),
        ..ProptestConfig::with_cases(64)
    })]

    #[test]
    fn conservation_engine_fuzz_regression_preserves_ledger_invariant(
        seed_index in 0_usize..usize::MAX,
    ) {
        let cases = seed_cases();
        replay_seed(&cases[seed_index % cases.len()]);
    }
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("protocol crate should be nested beneath the repository")
        .to_path_buf()
}
