#![no_main]

//! Fuzz harness for the conservation matching engine.
//!
//! Arbitrary inputs decode into (region bytes, corpus bytes, option bits)
//! and drive the public `audit_conservation` API over synthetic two-tree
//! inputs. The harness asserts no panic plus the §9.2 ledger identity for
//! every completed run; any early envelope error must be a sanctioned
//! resource-limit failure.

use libfuzzer_sys::fuzz_target;
use srcmv_core::{
    audit_conservation, AuditFile, ConservationBudget, ConservationInput, ConservationOptions,
    CoreError,
};

fuzz_target!(|data: &[u8]| {
    let options_bit = data.first().copied().unwrap_or_default();
    let strict_uniqueness = options_bit & 1 != 0;
    let count_formatting = options_bit & 2 != 0;

    // Decode two length-prefixed byte strings after the option header so
    // arbitrary corpora reach extraction and matching unharmed.
    let Ok((region_bytes, corpus_bytes)) = decode_pair(data.get(1..).unwrap_or_default()) else {
        return;
    };

    let min_fragment = 16_u64.max(region_bytes.len().min(64) as u64);
    let Ok(options) = ConservationOptions::new(
        min_fragment * 2,
        min_fragment,
        strict_uniqueness,
        count_formatting,
    ) else {
        return;
    };
    let budget = ConservationBudget::default();

    // Base tree: the region bytes embedded in a modified file plus a deleted
    // twin. Head tree: the raw corpus as a created file. Digests only feed
    // classification equality, so fabricated distinct digests are sound.
    let base_path = b"base/changed.txt".to_vec();
    let deleted_path = b"base/vanished.bin".to_vec();
    let head_path = b"head/corpus.bin".to_vec();
    let mut changed = b"frame\n".to_vec();
    changed.extend_from_slice(&region_bytes);
    changed.extend_from_slice(b"\ntrailer");

    let base_digest = digest_for(&changed);
    let deleted_digest = digest_for(&region_bytes);
    let head_digest = digest_for(&corpus_bytes);

    let base_files = [
        AuditFile {
            path: &base_path,
            bytes: &changed,
            digest: &base_digest,
        },
        AuditFile {
            path: &deleted_path,
            bytes: &region_bytes,
            digest: &deleted_digest,
        },
    ];
    let head_files = [AuditFile {
        path: &head_path,
        bytes: &corpus_bytes,
        digest: &head_digest,
    }];

    if let Ok(outcome) = audit_conservation(ConservationInput {
        base: &base_files,
        head: &head_files,
        options,
        budget,
    }) {
        let totals = outcome.totals;
        let terms = totals.conserved_exact_bytes
            + totals.conserved_fragments_bytes
            + totals.unmatched_substantial_bytes
            + totals.insubstantial_residue_bytes
            + totals.formatting_only_bytes
            + totals.ambiguous_common_bytes
            + totals.unanalyzed_bytes;
        assert_eq!(terms, totals.removed_bytes, "§9.2 identity must always hold");
        assert_eq!(
            totals.conserved_bytes,
            totals.conserved_exact_bytes + totals.conserved_fragments_bytes
        );
        assert!(outcome.totals.new_bytes <= corpus_bytes.len() as u64);
        for record in &outcome.records {
            assert!(
                record
                    .destinations
                    .iter()
                    .all(|destination| destination.byte_range.1 > destination.byte_range.0)
            );
        }
    } else {
        // Only the pre-match resource-limit boundary may error out (§9.3).
        // Re-run with unlimited memory budgets to surface any other panic.
        let generous = ConservationBudget::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX, 2);
        match audit_conservation(ConservationInput {
            base: &base_files,
            head: &head_files,
            options,
            budget: generous,
        }) {
            Ok(_) => {}
            Err(CoreError::ResourceLimitExceeded { .. }) => {
                panic!("generous budget still hit a resource limit");
            }
            Err(error) => panic!("unexpected engine error: {error:?}"),
        }
    }
});

/// Decodes `(first string, second string)` from a length-prefixed layout:
/// two little-endian u16 lengths followed by that many payload bytes.
fn decode_pair(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ()> {
    if data.len() < 4 {
        return Err(());
    }
    let first_len = u16::from_le_bytes([data[0], data[1]]) as usize;
    let rest = data.get(2..).ok_or(())?;
    if rest.len() < first_len + 2 {
        return Err(());
    }
    let second_len = u16::from_le_bytes([
        rest[first_len],
        *rest.get(first_len + 1).ok_or(())?,
    ]) as usize;
    let tail = rest.get(first_len + 2..).ok_or(())?;
    let second = tail.get(..second_len).ok_or(())?;
    Ok((
        rest.get(..first_len).ok_or(())?.to_vec(),
        second.to_vec(),
    ))
}

/// Deterministic fabricated digest; classification compares digests only for
/// equality, while the engine verifies all matched bytes directly.
fn digest_for(bytes: &[u8]) -> srcmv_core::Sha256Digest {
    let mut state = 0xcbf2_9ce4_8422_2325_u64 ^ bytes.len() as u64;
    for &byte in bytes {
        state ^= u64::from(byte);
        state = state.wrapping_mul(0x100_0000_01b3);
    }
    let mut digest = [0_u8; 32];
    for (index, slot) in digest.chunks_mut(8).enumerate() {
        let word = state
            .wrapping_add(index as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let encoded = word.to_le_bytes();
        slot.copy_from_slice(&encoded[..slot.len()]);
    }
    srcmv_core::Sha256Digest(digest)
}
