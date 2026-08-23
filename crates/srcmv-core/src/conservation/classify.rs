//! Pure path classification over sorted base and head projections.
//!
//! Implements the §7 classification table: `unchanged` (same path, equal
//! digest), `modified` (same path, different digest), `deleted` (path only in
//! base), and `created` (path only in head). The table expresses itself
//! structurally through [`TreeDiff`]: modified pairs drive extraction,
//! deleted base indices become whole-file regions, head indices absent from
//! [`TreeDiff::modified`] are created files, and everything else is
//! unchanged. Case sensitivity follows the captured path spelling; no folding
//! happens. Both input slices must be sorted by normalized relative path
//! bytes, which the engine guarantees by sorting defensively before calling.

use std::cmp::Ordering;

use super::AuditFile;

/// Merged classification of two path-sorted trees.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TreeDiff {
    /// Modified pairs as `(base index, head index)` in ascending path order.
    pub modified: Vec<(usize, usize)>,
    /// Deleted base indices in ascending path order.
    pub deleted: Vec<usize>,
    /// Created head indices in ascending path order.
    pub created: Vec<usize>,
}

/// Classifies `base` against `head` with a merge walk over both slices.
///
/// Both slices must be sorted by path bytes; equal paths pair up and resolve
/// through digest equality alone — equal digests are unchanged even if the
/// bytes differ, because the caller's digests are the classification
/// authority.
pub(crate) fn classify(base: &[AuditFile<'_>], head: &[AuditFile<'_>]) -> TreeDiff {
    let mut diff = TreeDiff::default();
    let mut base_index = 0_usize;
    let mut head_index = 0_usize;
    while base_index < base.len() && head_index < head.len() {
        match base[base_index].path.cmp(head[head_index].path) {
            Ordering::Less => {
                diff.deleted.push(base_index);
                base_index += 1;
            }
            Ordering::Greater => {
                diff.created.push(head_index);
                head_index += 1;
            }
            Ordering::Equal => {
                if base[base_index].digest != head[head_index].digest {
                    diff.modified.push((base_index, head_index));
                }
                base_index += 1;
                head_index += 1;
            }
        }
    }
    diff.deleted.extend(base_index..base.len());
    diff.created.extend(head_index..head.len());
    diff
}

#[cfg(test)]
mod tests {
    use super::{TreeDiff, classify};
    use crate::Sha256Digest;
    use crate::conservation::test_support::audit_file;

    #[test]
    fn classification_should_follow_the_documented_table() {
        let base_digests = [
            Sha256Digest([1; 32]),
            Sha256Digest([2; 32]),
            Sha256Digest([3; 32]),
        ];
        let head_digests = [
            Sha256Digest([4; 32]),
            Sha256Digest([1; 32]),
            Sha256Digest([5; 32]),
        ];
        let base = vec![
            audit_file(b"same.txt", b"v1", &base_digests[0]),
            audit_file(b"touched.txt", b"old", &base_digests[1]),
            audit_file(b"vanished.txt", b"gone", &base_digests[2]),
        ];
        let head = vec![
            audit_file(b"arrived.txt", b"new", &head_digests[0]),
            audit_file(b"same.txt", b"v1", &head_digests[1]),
            audit_file(b"touched.txt", b"rewritten at length", &head_digests[2]),
        ];

        let diff = classify(&base, &head);

        assert_eq!(diff.modified, vec![(1_usize, 2_usize)]);
        assert_eq!(diff.deleted, vec![2_usize]);
        assert_eq!(diff.created, vec![0_usize]);
    }

    #[test]
    fn classification_should_treat_equal_digests_as_unchanged_even_when_bytes_differ() {
        // The digest is the authority: callers that lie about digests get an
        // unchanged pair, never extraction work based on stale bytes.
        let shared = Sha256Digest([7; 32]);
        let base = vec![audit_file(b"a.txt", b"bytes-from-the-base-tree", &shared)];
        let head = vec![audit_file(b"a.txt", b"different bytes entirely", &shared)];

        let diff = classify(&base, &head);

        assert!(diff.modified.is_empty());
        assert!(diff.deleted.is_empty());
        assert!(diff.created.is_empty());
    }

    #[test]
    fn classification_should_handle_created_and_deleted_edges() {
        let base_digest = Sha256Digest([11; 32]);
        let head_digest = Sha256Digest([12; 32]);
        let base = vec![audit_file(b"only-base.txt", b"x", &base_digest)];
        let head = vec![audit_file(b"only-head.txt", b"y", &head_digest)];

        let diff = classify(&base, &head);

        assert_eq!(diff.deleted, vec![0]);
        assert_eq!(diff.created, vec![0]);
        assert!(diff.modified.is_empty());
    }

    #[test]
    fn classification_should_handle_empty_trees_and_identical_trees() {
        let empty_base: Vec<crate::conservation::AuditFile<'static>> = Vec::new();
        let empty_head: Vec<crate::conservation::AuditFile<'static>> = Vec::new();
        let empty = classify(&empty_base, &empty_head);
        assert_eq!(empty, TreeDiff::default());

        let shared = Sha256Digest([9; 32]);
        let files = vec![audit_file(b"a.txt", b"same", &shared)];
        let identical = classify(&files, &files);
        assert!(identical.modified.is_empty());
        assert!(identical.deleted.is_empty());
        assert!(identical.created.is_empty());
    }
}
