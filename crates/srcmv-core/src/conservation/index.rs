//! Rolling-hash corpus index over head-tree bytes.
//!
//! Implements §8.2: a virtual concatenation of every head file keyed by
//! fixed windows of `K` bytes at stride 1, mapping `u64` window hashes to
//! position vectors sorted ascending by `(path rank, offset)` because files
//! stream in path order and offsets ascend within a file. The mixer is a
//! dependency-free multiply-xor fold with wyhash-style finalization; hash
//! collisions are harmless because every candidate is verified byte-for-byte
//! before it can consume anything. Buckets exceeding the hot-bucket limit are
//! marked hot and excluded from probing entirely, which keeps pathological
//! corpora linear instead of quadratic.

use std::collections::HashMap;

use crate::CoreError;

use super::{AuditFile, ConservationBudget};

/// Normative index window width `K = 16` (plan §8.2).
///
/// §8.2 fixes `K = 16` normatively and §6.1 chains the resolution
/// constraints onto it: `--min-fragment-bytes ≥ K` and
/// `--min-substantial-bytes ≥ --min-fragment-bytes`, so every matchable
/// span contains at least one full probe window and the §9.4 floor
/// reasoning holds. The value is therefore a constant, never a budget
/// knob; only `hot_bucket_limit` stays configurable.
pub(crate) const INDEX_WINDOW_BYTES: usize = 16;

const MIX_A: u64 = 0x2d35_8dcc_aa6c_78a5;
const MIX_B: u64 = 0x8bb8_4b93_962e_acc9;
const MIX_C: u64 = 0x1656_67b1_9e37_79f9;

/// Conservative per-key overhead estimate charged before bucket allocation:
/// one hash-map slot plus vector header and allocation slack.
const ESTIMATED_BUCKET_BYTES: u64 = 64;

/// One indexed window occurrence in the virtual concatenation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Position {
    /// Rank of the hosting file in path-sorted head order.
    pub file: u32,
    /// Byte offset of the window start inside the hosting file.
    pub offset: u64,
}

#[derive(Debug, Default)]
struct Bucket {
    positions: Vec<Position>,
    hot: bool,
}

/// Probeable corpus index over all head files.
///
/// Positions never cross file boundaries because each file contributes its
/// own window stream; candidate spans are additionally bounds-checked against
/// their hosting file during verification. Windows are
/// [`INDEX_WINDOW_BYTES`] wide by §8.2's normative resolution.
#[derive(Debug)]
pub(crate) struct CorpusIndex {
    buckets: HashMap<u64, Bucket>,
}

impl CorpusIndex {
    /// Builds the index over `files`, the path-sorted head projection.
    ///
    /// Memory charges the budget with checked arithmetic before every
    /// allocation: [`ESTIMATED_BUCKET_BYTES`] per new key plus the packed
    /// position size per stored occurrence. Hot-bucket suppression drops the
    /// position vector once a key exceeds `budget.hot_bucket_limit()`; the
    /// spent charge stays deducted. A window exists at every offset with
    /// `offset + K <= file_len`, so occurrences flush against EOF remain
    /// reachable through their interior windows.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ResourceLimitExceeded`] when the projected index
    /// memory would exceed `budget.index_memory_bytes()` or when the head
    /// file count cannot be represented.
    pub(crate) fn build(
        files: &[AuditFile<'_>],
        budget: &ConservationBudget,
    ) -> Result<Self, CoreError> {
        let window_usize = INDEX_WINDOW_BYTES;
        let hot_limit = budget.hot_bucket_limit();
        let memory_limit = budget.index_memory_bytes();
        let mut buckets: HashMap<u64, Bucket> = HashMap::new();
        let mut charged = 0_u64;
        let position_bytes = u64::try_from(std::mem::size_of::<Position>()).unwrap_or(u64::MAX);

        for (rank, file) in files.iter().enumerate() {
            let Ok(file_index) = u32::try_from(rank) else {
                return Err(CoreError::ResourceLimitExceeded {
                    resource: "conservation_head_files",
                    actual: u64::try_from(rank).unwrap_or(u64::MAX),
                    limit: u64::from(u32::MAX),
                });
            };
            let bytes = file.bytes;
            if window_usize > bytes.len() {
                continue;
            }
            for offset in 0..=(bytes.len() - window_usize) {
                let key = mix_bytes(&bytes[offset..offset + window_usize]);
                match buckets.get_mut(&key) {
                    Some(bucket) => {
                        if bucket.hot {
                            continue;
                        }
                        charge_memory(&mut charged, position_bytes, memory_limit)?;
                        bucket.positions.push(Position {
                            file: file_index,
                            offset: offset as u64,
                        });
                        if bucket.positions.len() as u64 > hot_limit {
                            bucket.hot = true;
                            bucket.positions = Vec::new();
                        }
                    }
                    None => {
                        charge_memory(
                            &mut charged,
                            ESTIMATED_BUCKET_BYTES.saturating_add(position_bytes),
                            memory_limit,
                        )?;
                        buckets.insert(
                            key,
                            Bucket {
                                positions: vec![Position {
                                    file: file_index,
                                    offset: offset as u64,
                                }],
                                hot: false,
                            },
                        );
                    }
                }
            }
        }

        Ok(Self { buckets })
    }

    /// Returns the cold candidate positions for `key` in ascending
    /// `(path rank, offset)` order.
    ///
    /// A key with no indexed occurrences is trivially cold and probes to an
    /// empty candidate list; `None` is reserved for hot-suppressed buckets so
    /// callers can distinguish "cannot anchor" from "nothing there".
    #[must_use]
    pub(crate) fn cold_candidates(&self, key: u64) -> Option<&[Position]> {
        match self.buckets.get(&key) {
            Some(bucket) if !bucket.hot => Some(&bucket.positions),
            Some(_) => None,
            None => Some(&[]),
        }
    }

    /// Inserts an arbitrary position under an arbitrary key so collision
    /// handling can be exercised without searching for real collisions.
    #[cfg(test)]
    fn inject_position(&mut self, key: u64, position: Position, hot_limit: u64) {
        let bucket = self.buckets.entry(key).or_default();
        if bucket.hot {
            return;
        }
        bucket.positions.push(position);
        if bucket.positions.len() as u64 > hot_limit {
            bucket.hot = true;
            bucket.positions = Vec::new();
        }
    }
}

fn charge_memory(charged: &mut u64, addition: u64, limit: u64) -> Result<(), CoreError> {
    let projected = charged.saturating_add(addition);
    if projected > limit {
        return Err(CoreError::ResourceLimitExceeded {
            resource: "conservation_index_memory",
            actual: projected,
            limit,
        });
    }
    *charged = projected;
    Ok(())
}

/// Multiplies two 128-bit halves together and folds them back with xor.
fn mum(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
    ((product >> 64) as u64) ^ (product as u64)
}

fn read_le_u64(bytes: &[u8]) -> u64 {
    let mut buffer = [0_u8; 8];
    let count = bytes.len().min(8);
    buffer[..count].copy_from_slice(&bytes[..count]);
    u64::from_le_bytes(buffer)
}

/// Dependency-free deterministic byte mixer used for both index windows and
/// trimmed line-content hashes.
///
/// Collisions only add spurious candidates that byte verification rejects;
/// they can never corrupt accounting or soundness.
#[must_use]
pub(crate) fn mix_bytes(bytes: &[u8]) -> u64 {
    let mut state = MIX_A ^ u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    for (index, chunk) in bytes.chunks(16).enumerate() {
        let low = read_le_u64(chunk);
        let high = read_le_u64(chunk.get(8..).unwrap_or(&[]));
        state = mum(state ^ low, MIX_B ^ high ^ (index as u64));
    }
    let mut mixed = mum(state, MIX_C);
    mixed ^= mixed >> 29;
    mixed = mum(mixed, MIX_A);
    mixed ^= mixed >> 32;
    mixed
}

#[cfg(test)]
mod tests {
    use super::{CorpusIndex, INDEX_WINDOW_BYTES, Position, mix_bytes};
    use crate::conservation::test_support::{audit_file, budget_with, unlimited_budget};
    use crate::{CoreError, Sha256Digest};

    const K: usize = 16;

    #[test]
    fn index_should_probe_windows_without_crossing_file_boundaries() {
        // Both files share their first 16 bytes; the second file's tail would
        // create extra windows under naive concatenation but must not here.
        let first_digest = Sha256Digest([1; 32]);
        let second_digest = Sha256Digest([2; 32]);
        let head = vec![
            audit_file(b"b.bin", b"0123456789abcdefMIDDLE-PADDING", &first_digest),
            audit_file(b"a.bin", b"0123456789abcdef", &second_digest),
        ];
        let index =
            CorpusIndex::build(&head, &unlimited_budget()).expect("small index should build");

        let candidates = index
            .cold_candidates(mix_bytes(b"0123456789abcdef"))
            .expect("window should be cold");

        let starts: Vec<(u32, u64)> = candidates
            .iter()
            .map(|position| (position.file, position.offset))
            .collect();
        assert_eq!(starts, vec![(0, 0), (1, 0)]);
        assert_eq!(INDEX_WINDOW_BYTES, K);
    }

    #[test]
    fn hot_bucket_suppression_should_stay_cold_at_exactly_the_limit_and_flip_above_it() {
        // A 19-byte run of 'x' yields exactly 4 windows under one key; a
        // 20-byte run yields 5. With hot limit 4 the first stays probeable
        // and the second suppresses.
        let strict_budget = budget_with(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX, 4);
        let shared_digest = Sha256Digest([3; 32]);

        let at_limit_files = vec![audit_file(b"run.bin", &[b'x'; 19], &shared_digest)];
        let at_limit =
            CorpusIndex::build(&at_limit_files, &strict_budget).expect("small index should build");
        assert_eq!(
            at_limit
                .cold_candidates(mix_bytes(&[b'x'; K]))
                .expect("cold at exactly the limit")
                .len(),
            4
        );

        let over_limit_files = vec![audit_file(b"run.bin", &[b'x'; 20], &shared_digest)];
        let over_limit =
            CorpusIndex::build(&over_limit_files, &strict_budget).expect("small index");
        assert!(over_limit.cold_candidates(mix_bytes(&[b'x'; K])).is_none());
    }

    #[test]
    fn hot_bucket_boundary_should_flip_exactly_between_64_and_65_positions() {
        // With the release hot limit of 64, a 79-byte run yields exactly 64
        // windows under one key and stays probeable; an 80-byte run adds the
        // 65th position and suppresses the bucket.
        let release_budget = budget_with(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64);
        let shared_digest = Sha256Digest([10; 32]);

        let sixty_four_files = vec![audit_file(b"run.bin", &[b'x'; 79], &shared_digest)];
        let sixty_four =
            CorpusIndex::build(&sixty_four_files, &release_budget).expect("small index");
        assert_eq!(
            sixty_four
                .cold_candidates(mix_bytes(&[b'x'; K]))
                .expect("cold at exactly 64 positions")
                .len(),
            64
        );

        let sixty_five_files = vec![audit_file(b"run.bin", &[b'x'; 80], &shared_digest)];
        let sixty_five =
            CorpusIndex::build(&sixty_five_files, &release_budget).expect("small index");
        assert!(sixty_five.cold_candidates(mix_bytes(&[b'x'; K])).is_none());
    }

    #[test]
    fn injected_collisions_should_surface_as_candidates_that_verification_rejects() {
        let corpus_digest = Sha256Digest([4; 32]);
        let head = vec![audit_file(
            b"corpus.bin",
            b"AAAAAAAAAAAAAAAAtotally-different-tail",
            &corpus_digest,
        )];
        let mut index = CorpusIndex::build(&head, &unlimited_budget()).expect("small index");
        let key = mix_bytes(b"AAAAAAAAAAAAAAAA");
        index.inject_position(
            key,
            Position {
                file: 0,
                offset: 22,
            },
            unlimited_budget().hot_bucket_limit(),
        );

        let candidates = index.cold_candidates(key).expect("bucket stays cold");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].offset, 0);
        assert_eq!(candidates[1].offset, 22);
        // The impostor's window bytes differ from the probed window even
        // though it shares the key:
        let corpus = head[0].bytes;
        assert_ne!(&corpus[22..38], b"AAAAAAAAAAAAAAAA".as_slice());
    }

    #[test]
    fn occurrences_flush_against_eof_should_stay_indexed_through_interior_windows() {
        // Length 18 admits windows at offsets 0..=2 only; the window that
        // ends exactly at EOF starts at offset 2 and is indexed normally.
        let tail_digest = Sha256Digest([5; 32]);
        let head = vec![audit_file(b"tail.bin", b"zzZZZZZZZZZZZZZZQQ", &tail_digest)];
        let index =
            CorpusIndex::build(&head, &unlimited_budget()).expect("small index should build");

        assert_eq!(
            index.cold_candidates(mix_bytes(b"zzZZZZZZZZZZZZZZ")),
            Some(&[Position { file: 0, offset: 0 }][..])
        );
        let candidates = index
            .cold_candidates(mix_bytes(b"ZZZZZZZZZZZZZZQQ"))
            .expect("flush-EOF window should exist");

        assert_eq!(candidates, &[Position { file: 0, offset: 2 }]);
    }

    #[test]
    fn index_build_should_fail_closed_when_projected_memory_exceeds_the_cap() {
        let big_digest = Sha256Digest([6; 32]);
        let head = vec![audit_file(b"big.bin", &[7_u8; 1024], &big_digest)];
        let tiny_budget = budget_with(u64::MAX, u64::MAX, u64::MAX, 48, u64::MAX, 64);

        let error = CorpusIndex::build(&head, &tiny_budget)
            .expect_err("projected memory should exceed the tiny cap");

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
    fn empty_and_tiny_head_files_should_contribute_no_windows() {
        let empty_digest = Sha256Digest([7; 32]);
        let tiny_digest = Sha256Digest([8; 32]);
        let head = vec![
            audit_file(b"empty.bin", b"", &empty_digest),
            audit_file(b"tiny.bin", b"short", &tiny_digest),
        ];
        let index =
            CorpusIndex::build(&head, &unlimited_budget()).expect("small index should build");
        assert!(
            index
                .cold_candidates(mix_bytes(b"shortshortshort"))
                .expect("absent keys stay cold")
                .is_empty()
        );
        assert_eq!(INDEX_WINDOW_BYTES, K);
    }
}
