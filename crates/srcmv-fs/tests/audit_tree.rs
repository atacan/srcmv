//! Integration coverage for conservation-audit tree ingestion (plan §7).

use std::fs;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;

use sha2::{Digest as _, Sha256};
use srcmv_core::Sha256Digest;
use srcmv_fs::{
    AuditTreeFile, AuditTreeLimits, FsError, SkipReason, SkippedInput, audit_tree_faults,
    read_audit_tree, read_audit_tree_from_manifest,
};
use tempfile::TempDir;

const UNBOUNDED: u64 = u64::MAX;

fn audit_limits(
    max_files: u64,
    file_bytes: u64,
    total_bytes: u64,
    path_bytes: u64,
) -> AuditTreeLimits {
    AuditTreeLimits::new(
        max_files,
        file_bytes,
        total_bytes,
        path_bytes,
        UNBOUNDED,
        UNBOUNDED,
    )
}

fn default_limits() -> AuditTreeLimits {
    audit_limits(UNBOUNDED, UNBOUNDED, UNBOUNDED, UNBOUNDED)
}

fn manifest_limits(entries: u64, manifest_bytes: u64) -> AuditTreeLimits {
    AuditTreeLimits::new(
        UNBOUNDED,
        UNBOUNDED,
        UNBOUNDED,
        UNBOUNDED,
        entries,
        manifest_bytes,
    )
}

fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(Sha256::digest(bytes).into())
}

fn paths(files: &[AuditTreeFile]) -> Vec<String> {
    files.iter().map(|file| file.path.value.clone()).collect()
}

fn skip_paths(skips: &[SkippedInput]) -> Vec<(String, SkipReason)> {
    skips
        .iter()
        .map(|skip| (skip.path.value.clone(), skip.reason))
        .collect()
}

fn write_file(root: &Path, relative: &str, contents: &[u8]) {
    let full = root.join(relative);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("fixture parent should be created");
    }
    fs::write(full, contents).expect("fixture should be written");
}

fn expect_limit_error(error: FsError, resource: &'static str, actual: u64, limit: u64) {
    assert_eq!(
        error,
        FsError::ResourceLimitExceeded {
            resource,
            actual,
            limit,
        }
    );
}

#[test]
fn read_audit_tree_returns_exact_paths_bytes_and_digests() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "src/one.txt", b"alpha\n");
    write_file(root.path(), "two.txt", b"beta");
    fs::create_dir(root.path().join("empty-dir")).expect("empty directory should be created");

    let (files, skips) =
        read_audit_tree(root.path(), default_limits()).expect("tree should be read");

    assert!(skips.is_empty());
    assert_eq!(paths(&files), vec!["src/one.txt", "two.txt"]);
    assert_eq!(&*files[0].bytes, b"alpha\n");
    assert_eq!(files[0].digest, digest(b"alpha\n"));
    assert_eq!(&*files[1].bytes, b"beta");
    assert_eq!(files[1].digest, digest(b"beta"));
}

#[test]
fn read_audit_tree_sorts_outputs_by_normalized_path_bytes_including_case() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "bravo/inner.txt", b"b");
    write_file(root.path(), "alpha/inner.txt", b"ai");
    write_file(root.path(), "alpha.txt", b"a");
    write_file(root.path(), "Bravo.txt", b"B");

    let (files, _) = read_audit_tree(root.path(), default_limits()).expect("tree should be read");

    // Byte order: 'B' (0x42) sorts before every lowercase name, and
    // "alpha.txt" precedes "alpha/inner.txt" because '.' (0x2E) sorts before
    // '/' (0x2F). Same-level case-colliding spellings cannot coexist on
    // case-insensitive CI volumes; the manifest suite covers no-folding at the
    // path level instead.
    assert_eq!(
        paths(&files),
        vec![
            "Bravo.txt",
            "alpha.txt",
            "alpha/inner.txt",
            "bravo/inner.txt"
        ]
    );
}

#[test]
fn read_audit_tree_skips_exactly_three_root_control_names_and_audits_other_dotfiles() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), ".git/HEAD", b"git");
    write_file(root.path(), ".srcmv/journal", b"srcmv");
    write_file(root.path(), ".codesplice/data", b"legacy");
    write_file(root.path(), ".editorconfig", b"editor");
    write_file(root.path(), "sub/.git/config", b"nested git is content");

    let (files, skips) =
        read_audit_tree(root.path(), default_limits()).expect("tree should be read");

    assert!(skips.is_empty());
    // Nested control-named directories and other dotfiles are audited content.
    assert_eq!(paths(&files), vec![".editorconfig", "sub/.git/config"]);
}

#[test]
fn read_audit_tree_accepts_empty_files_and_empty_directories() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "empty", b"");
    fs::create_dir_all(root.path().join("nested/deeper")).expect("empty dirs should be created");

    let (files, skips) =
        read_audit_tree(root.path(), default_limits()).expect("tree should be read");

    assert!(skips.is_empty());
    assert_eq!(paths(&files), vec!["empty"]);
    assert_eq!(&*files[0].bytes, b"");
    assert_eq!(files[0].digest, digest(b""));
}

#[test]
fn read_audit_tree_rejects_root_level_and_nested_symlinks() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "real.txt", b"real");
    write_file(root.path(), "sub/target.txt", b"target");
    symlink("real.txt", root.path().join("top-link")).expect("symlink should be created");
    symlink("../real.txt", root.path().join("sub/nested-link"))
        .expect("nested symlink should be created");

    let top_error =
        read_audit_tree(root.path(), default_limits()).expect_err("root symlink must fail");
    assert_eq!(
        top_error,
        FsError::SymlinkNotAllowed {
            path: "sub/nested-link".to_owned(),
        }
    );

    fs::remove_file(root.path().join("sub/nested-link")).expect("fixture should be removed");
    let error = read_audit_tree(root.path(), default_limits()).expect_err("symlink must fail");
    assert_eq!(
        error,
        FsError::SymlinkNotAllowed {
            path: "top-link".to_owned(),
        }
    );
}

#[test]
fn read_audit_tree_rejects_special_files() {
    let root = TempDir::new().expect("tree should be created");
    UnixListener::bind(root.path().join("socket")).expect("unix socket fixture should be created");

    let error = read_audit_tree(root.path(), default_limits())
        .expect_err("special file must fail the audit");

    assert_eq!(
        error,
        FsError::UnsupportedFileType {
            path: "socket".to_owned(),
        }
    );
}

#[test]
fn read_audit_tree_rejects_missing_root_with_an_io_error() {
    let root = TempDir::new().expect("tree should be created");
    let missing = root.path().join("does-not-exist");

    let error = read_audit_tree(&missing, default_limits()).expect_err("missing root must fail");

    assert!(matches!(
        error,
        FsError::Io {
            operation: "canonicalize_workspace",
            ..
        }
    ));
}

#[test]
fn read_audit_tree_rejects_a_file_root() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "plain-file", b"bytes");

    let error = read_audit_tree(&root.path().join("plain-file"), default_limits())
        .expect_err("file root must fail");

    assert_eq!(error, FsError::WorkspaceRootNotDirectory);
}

#[test]
fn read_audit_tree_does_not_modify_the_input_tree() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "source", b"immutable\n");
    let before_entries = directory_entries(root.path());
    let before_metadata =
        fs::metadata(root.path().join("source")).expect("metadata should be readable");

    let _ = read_audit_tree(root.path(), default_limits()).expect("tree should be read");

    assert_eq!(directory_entries(root.path()), before_entries);
    let after_metadata =
        fs::metadata(root.path().join("source")).expect("metadata should remain readable");
    assert_eq!(after_metadata.len(), before_metadata.len());
    assert_eq!(after_metadata.mtime(), before_metadata.mtime());
}

#[test]
fn read_audit_tree_enforces_the_file_count_limit_below_at_and_above() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "one", b"1");
    write_file(root.path(), "two", b"2");
    write_file(root.path(), "three", b"3");

    let below = read_audit_tree(
        root.path(),
        audit_limits(4, UNBOUNDED, UNBOUNDED, UNBOUNDED),
    )
    .expect("usage below the limit should pass");
    assert_eq!(below.0.len(), 3);
    let at = read_audit_tree(
        root.path(),
        audit_limits(3, UNBOUNDED, UNBOUNDED, UNBOUNDED),
    )
    .expect("usage at the limit should pass");
    assert_eq!(at.0.len(), 3);
    let above = read_audit_tree(
        root.path(),
        audit_limits(2, UNBOUNDED, UNBOUNDED, UNBOUNDED),
    )
    .expect_err("usage above the limit must fail");
    expect_limit_error(above, "audit_tree_files", 3, 2);
}

#[test]
fn read_audit_tree_enforces_the_per_file_byte_limit_below_at_and_above() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "source", b"12345678");

    let below = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, 9, UNBOUNDED, UNBOUNDED),
    )
    .expect("file below the limit should pass");
    assert_eq!(below.0[0].bytes.len(), 8);
    let at = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, 8, UNBOUNDED, UNBOUNDED),
    )
    .expect("file at the limit should pass");
    assert_eq!(at.0[0].bytes.len(), 8);
    let above = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, 7, UNBOUNDED, UNBOUNDED),
    )
    .expect_err("file above the limit must fail");
    expect_limit_error(above, "audit_tree_file_bytes", 8, 7);
}

#[test]
fn read_audit_tree_enforces_the_total_byte_limit_below_at_and_above() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "one", b"12345");
    write_file(root.path(), "two", b"67890");

    let below = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, UNBOUNDED, 11, UNBOUNDED),
    )
    .expect("total below the limit should pass");
    assert_eq!(below.0.len(), 2);
    let at = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, UNBOUNDED, 10, UNBOUNDED),
    )
    .expect("total at the limit should pass");
    assert_eq!(at.0.len(), 2);
    let above = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, UNBOUNDED, 9, UNBOUNDED),
    )
    .expect_err("total above the limit must fail");
    expect_limit_error(above, "audit_tree_total_bytes", 10, 9);
}

#[test]
fn read_audit_tree_enforces_the_path_byte_limit_below_at_and_above() {
    let root = TempDir::new().expect("tree should be created");
    // "abcdefgh/ij" is 11 bytes of normalized relative path.
    write_file(root.path(), "abcdefgh/ij", b"bytes");

    let below = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, UNBOUNDED, UNBOUNDED, 12),
    )
    .expect("path below the limit should pass");
    assert_eq!(paths(&below.0), vec!["abcdefgh/ij"]);
    let at = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, UNBOUNDED, UNBOUNDED, 11),
    )
    .expect("path at the limit should pass");
    assert_eq!(paths(&at.0), vec!["abcdefgh/ij"]);
    let above = read_audit_tree(
        root.path(),
        audit_limits(UNBOUNDED, UNBOUNDED, UNBOUNDED, 10),
    )
    .expect_err("path above the limit must fail");
    expect_limit_error(above, "path_bytes", 11, 10);
}

#[test]
fn read_audit_tree_records_a_seam_forced_vanished_file_as_a_skip() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "gone.txt", b"vanished content");
    write_file(root.path(), "keep.txt", b"stable content");

    audit_tree_faults::install(audit_tree_faults::Faults {
        vanish: vec!["gone.txt".to_owned()],
        unstable: Vec::new(),
        type_flip: Vec::new(),
    });
    let first = read_audit_tree(root.path(), default_limits());
    let second = read_audit_tree(root.path(), default_limits());
    audit_tree_faults::clear();

    for result in [first, second] {
        let (files, skips) = result.expect("vanished input must skip, not abort");
        assert_eq!(paths(&files), vec!["keep.txt"]);
        assert_eq!(
            skip_paths(&skips),
            vec![("gone.txt".to_owned(), SkipReason::Vanished)]
        );
    }
}

#[test]
fn read_audit_tree_records_three_unstable_reads_as_a_skip() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "flaky.txt", b"churning content");
    write_file(root.path(), "keep.txt", b"stable content");

    audit_tree_faults::install(audit_tree_faults::Faults {
        vanish: Vec::new(),
        unstable: vec!["flaky.txt".to_owned()],
        type_flip: Vec::new(),
    });
    let result = read_audit_tree(root.path(), default_limits());
    audit_tree_faults::clear();

    let (files, skips) = result.expect("unstable input must skip after three attempts");
    assert_eq!(paths(&files), vec!["keep.txt"]);
    assert_eq!(
        skip_paths(&skips),
        vec![("flaky.txt".to_owned(), SkipReason::UnstableReads)]
    );
}

#[test]
fn read_audit_tree_records_a_seam_forced_type_flip_as_an_unstable_skip() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "flipped.txt", b"was regular when listed");
    write_file(root.path(), "keep.txt", b"stable content");

    audit_tree_faults::install(audit_tree_faults::Faults {
        vanish: Vec::new(),
        unstable: Vec::new(),
        type_flip: vec!["flipped.txt".to_owned()],
    });
    let result = read_audit_tree(root.path(), default_limits());
    audit_tree_faults::clear();

    // A delete-and-recreate-with-different-type race is per-file instability:
    // it consumes all three attempts, then skips instead of aborting the walk.
    let (files, skips) = result.expect("mid-acquisition type flip must skip, not abort");
    assert_eq!(paths(&files), vec!["keep.txt"]);
    assert_eq!(
        skip_paths(&skips),
        vec![("flipped.txt".to_owned(), SkipReason::UnstableReads)]
    );
}

#[test]
fn read_audit_tree_records_multiple_skips_in_joint_sorted_order() {
    let root = TempDir::new().expect("tree should be created");
    // Depth-first discovery visits `a/k-gone.txt`, `a/x-flaky.txt`, then
    // root-level `b-gone.txt`, deliberately differing from byte-sorted order.
    write_file(root.path(), "a/k-gone.txt", b"vanished deeper");
    write_file(root.path(), "a/x-flaky.txt", b"churning content");
    write_file(root.path(), "b-gone.txt", b"vanished shallow");
    write_file(root.path(), "keep.txt", b"stable content");

    audit_tree_faults::install(audit_tree_faults::Faults {
        vanish: vec!["a/k-gone.txt".to_owned(), "b-gone.txt".to_owned()],
        unstable: vec!["a/x-flaky.txt".to_owned()],
        type_flip: Vec::new(),
    });
    let result = read_audit_tree(root.path(), default_limits());
    audit_tree_faults::clear();

    let (files, skips) = result.expect("multiple skips must be collected, not abort");
    assert_eq!(paths(&files), vec!["keep.txt"]);
    // Skips of different reasons are sorted jointly by path bytes, not grouped
    // by reason or left in discovery order.
    assert_eq!(
        skip_paths(&skips),
        vec![
            ("a/k-gone.txt".to_owned(), SkipReason::Vanished),
            ("a/x-flaky.txt".to_owned(), SkipReason::UnstableReads),
            ("b-gone.txt".to_owned(), SkipReason::Vanished),
        ]
    );
}

// APFS (macOS) rejects invalid-UTF-8 entry names at creation time with
// EILSEQ, so this fixture can only be fabricated on byte-transparent
// filesystems such as Linux's ext4.
#[cfg(target_os = "linux")]
#[test]
fn read_audit_tree_rejects_non_utf8_entry_names_with_the_entry_reason() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "keep.txt", b"stable content");
    fs::write(
        root.path().join(OsStr::from_bytes(b"bad\xffname")),
        b"bytes",
    )
    .expect("entry with non-UTF-8 name should be written");

    let error =
        read_audit_tree(root.path(), default_limits()).expect_err("non-UTF-8 names must fail");

    assert_eq!(
        error,
        FsError::InvalidPath {
            path: "bad\u{FFFD}name".to_owned(),
            reason: "entry_not_utf8",
        }
    );
}

#[test]
fn read_audit_tree_accepts_a_root_spelled_through_a_symlink() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "real/inner.txt", b"behind the root link");
    symlink("real", root.path().join("root-link")).expect("symlink should be created");

    let (files, skips) = read_audit_tree(&root.path().join("root-link"), default_limits())
        .expect("the canonicalized-once contract admits a symlinked root spelling");

    // The link itself is consumed by canonicalization; entries beneath the
    // canonical root keep their real relative spellings.
    assert!(skips.is_empty());
    assert_eq!(paths(&files), vec!["inner.txt"]);
    assert_eq!(&*files[0].bytes, b"behind the root link");
}

#[test]
fn audit_manifest_reads_commented_manifest_resolved_against_its_parent() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "src/one.txt", b"one");
    write_file(root.path(), "two.txt", b"two");
    write_file(root.path(), "ignored.txt", b"never listed");
    let manifest = "# touched files\r\n\r\nsrc/one.txt\ntwo.txt\r\n";
    fs::write(root.path().join("list.txt"), manifest).expect("manifest should be written");

    let (files, skips) =
        read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
            .expect("manifest should be read");

    assert!(skips.is_empty());
    // Comments and blank lines are ignored; CRLF endings tolerated; the
    // unlisted file contributes nothing.
    assert_eq!(paths(&files), vec!["src/one.txt", "two.txt"]);
}

#[test]
fn audit_manifest_resolves_paths_against_the_manifest_directory() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "sub/deeper/leaf.txt", b"leaf");
    write_file(root.path(), "sub/other.txt", b"other");
    fs::write(root.path().join("sub/list.txt"), "deeper/leaf.txt\n")
        .expect("manifest should be written");

    let (files, _) =
        read_audit_tree_from_manifest(&root.path().join("sub/list.txt"), default_limits())
            .expect("manifest should be read");

    // Paths keep their manifest spelling relative to the manifest's parent
    // (that directory is the tree root), while resolution reads from
    // `sub/deeper/leaf.txt` on disk.
    assert_eq!(paths(&files), vec!["deeper/leaf.txt"]);
    assert_eq!(&*files[0].bytes, b"leaf");
}

#[test]
fn audit_manifest_rejects_a_missing_entry_as_a_hard_error() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "present.txt", b"here");
    fs::write(root.path().join("list.txt"), "present.txt\nghost.txt\n")
        .expect("manifest should be written");

    let error = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
        .expect_err("missing entries must fail before any analysis");

    assert_eq!(
        error,
        FsError::PathNotFound {
            path: "ghost.txt".to_owned(),
        }
    );
}

#[test]
fn audit_manifest_rejects_invalid_path_spellings() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "real.txt", b"bytes");

    for (line, reason) in [
        ("../escape.txt", "path_parent_component"),
        ("/absolute.txt", "path_absolute"),
        ("double//slash.txt", "path_empty_component"),
        (".srcmv/control", "reserved_path"),
        ("trailing/", "path_empty_component"),
    ] {
        fs::write(root.path().join("list.txt"), format!("{line}\n"))
            .expect("manifest should be written");
        let error = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
            .expect_err("invalid spellings must fail");
        assert_eq!(
            error,
            FsError::InvalidPath {
                path: line.to_owned(),
                reason,
            },
            "{line}"
        );
    }
}

#[test]
fn audit_manifest_enforces_the_entry_count_limit_below_at_and_above() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "one.txt", b"1");
    write_file(root.path(), "two.txt", b"2");
    write_file(root.path(), "three.txt", b"3");
    fs::write(
        root.path().join("list.txt"),
        "one.txt\ntwo.txt\nthree.txt\n",
    )
    .expect("manifest should be written");

    let below =
        read_audit_tree_from_manifest(&root.path().join("list.txt"), manifest_limits(4, UNBOUNDED))
            .expect("usage below the limit should pass");
    assert_eq!(below.0.len(), 3);
    let at =
        read_audit_tree_from_manifest(&root.path().join("list.txt"), manifest_limits(3, UNBOUNDED))
            .expect("usage at the limit should pass");
    assert_eq!(at.0.len(), 3);
    let above =
        read_audit_tree_from_manifest(&root.path().join("list.txt"), manifest_limits(2, UNBOUNDED))
            .expect_err("usage above the limit must fail");
    expect_limit_error(above, "manifest_entries", 3, 2);
}

#[test]
fn audit_manifest_enforces_the_manifest_byte_limit_below_at_and_above() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "one.txt", b"1");
    let manifest_path = root.path().join("list.txt");
    fs::write(&manifest_path, "one.txt\n").expect("manifest should be written");
    let size = fs::metadata(&manifest_path)
        .expect("manifest metadata should be readable")
        .len();

    read_audit_tree_from_manifest(&manifest_path, manifest_limits(UNBOUNDED, size + 1))
        .expect("manifest below the limit should pass");
    read_audit_tree_from_manifest(&manifest_path, manifest_limits(UNBOUNDED, size))
        .expect("manifest at the limit should pass");
    let above = read_audit_tree_from_manifest(&manifest_path, manifest_limits(UNBOUNDED, size - 1))
        .expect_err("manifest above the limit must fail");
    expect_limit_error(above, "manifest_bytes", size, size - 1);
}

#[test]
fn audit_manifest_deduplicates_exact_duplicates_but_keeps_case_distinct_entries() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "dup.txt", b"duplicated listing");
    write_file(root.path(), "Alpha.txt", b"upper");
    write_file(root.path(), "alpha.txt", b"lower");
    fs::write(
        root.path().join("list.txt"),
        "dup.txt\ndup.txt\nAlpha.txt\nalpha.txt\n",
    )
    .expect("manifest should be written");

    let (files, _) = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
        .expect("manifest should be read");

    // The exact duplicate is ingested once; case-distinct spellings name
    // distinct paths and are never folded.
    assert_eq!(paths(&files), vec!["Alpha.txt", "alpha.txt", "dup.txt"]);
}

#[test]
fn audit_manifest_records_an_unstable_read_as_a_skip() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "flaky.txt", b"churning content");
    write_file(root.path(), "keep.txt", b"stable content");
    fs::write(root.path().join("list.txt"), "flaky.txt\nkeep.txt\n")
        .expect("manifest should be written");

    audit_tree_faults::install(audit_tree_faults::Faults {
        vanish: Vec::new(),
        unstable: vec!["flaky.txt".to_owned()],
        type_flip: Vec::new(),
    });
    let result = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits());
    audit_tree_faults::clear();

    let (files, skips) = result.expect("unstable input must skip after three attempts");
    assert_eq!(paths(&files), vec!["keep.txt"]);
    assert_eq!(
        skip_paths(&skips),
        vec![("flaky.txt".to_owned(), SkipReason::UnstableReads)]
    );
}

#[test]
fn audit_manifest_records_a_seam_forced_type_flip_as_an_unstable_skip() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "flaky.txt", b"churning content");
    write_file(root.path(), "keep.txt", b"stable content");
    fs::write(root.path().join("list.txt"), "flaky.txt\nkeep.txt\n")
        .expect("manifest should be written");

    audit_tree_faults::install(audit_tree_faults::Faults {
        vanish: Vec::new(),
        unstable: Vec::new(),
        type_flip: vec!["flaky.txt".to_owned()],
    });
    let result = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits());
    audit_tree_faults::clear();

    // The entry is still a regular file at every listing-time check, so the
    // mid-acquisition flip gets its remaining attempts and ends as a skip.
    let (files, skips) = result.expect("mid-acquisition type flip must skip, not abort");
    assert_eq!(paths(&files), vec!["keep.txt"]);
    assert_eq!(
        skip_paths(&skips),
        vec![("flaky.txt".to_owned(), SkipReason::UnstableReads)]
    );
}

#[test]
fn audit_manifest_rejects_symlinked_intermediate_components_as_hard_errors() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "inner/file.txt", b"behind link");
    symlink("inner", root.path().join("link")).expect("symlink should be created");
    fs::write(root.path().join("list.txt"), "link/file.txt\n").expect("manifest should be written");

    let error = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
        .expect_err("symlinked intermediate components must fail");

    // The §7 mechanical boundary rejects symlink traversal immediately rather
    // than burning the retry budget and ending as an UnstableReads skip.
    assert_eq!(
        error,
        FsError::SymlinkNotAllowed {
            path: "link/file.txt".to_owned(),
        }
    );
}

#[test]
fn audit_manifest_rejects_a_symlinked_final_component() {
    let root = TempDir::new().expect("tree should be created");
    write_file(root.path(), "real.txt", b"real bytes");
    symlink("real.txt", root.path().join("final-link")).expect("symlink should be created");
    fs::write(root.path().join("list.txt"), "final-link\n").expect("manifest should be written");

    let error = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
        .expect_err("a symlinked final component must fail");

    assert_eq!(
        error,
        FsError::SymlinkNotAllowed {
            path: "final-link".to_owned(),
        }
    );
}

#[test]
fn audit_manifest_rejects_a_socket_entry_as_an_unsupported_type() {
    let root = TempDir::new().expect("tree should be created");
    UnixListener::bind(root.path().join("socket")).expect("socket fixture should be created");
    fs::write(root.path().join("list.txt"), "socket\n").expect("manifest should be written");

    let error = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
        .expect_err("special-file entries must fail as unsupported types");

    // Like a fifo, a socket is neither a regular file, directory, nor symlink,
    // so it exercises the same listing-time UnsupportedFileType rejection.
    assert_eq!(
        error,
        FsError::UnsupportedFileType {
            path: "socket".to_owned(),
        }
    );
}

#[test]
fn audit_manifest_rejects_a_directory_entry_as_an_unsupported_type() {
    let root = TempDir::new().expect("tree should be created");
    fs::create_dir(root.path().join("subdir")).expect("directory fixture should be created");
    fs::write(root.path().join("list.txt"), "subdir\n").expect("manifest should be written");

    let error = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
        .expect_err("directory entries must fail as unsupported types");

    assert_eq!(
        error,
        FsError::UnsupportedFileType {
            path: "subdir".to_owned(),
        }
    );
}

#[test]
fn audit_manifest_rejects_non_utf8_manifest_bytes() {
    let root = TempDir::new().expect("tree should be created");
    fs::write(root.path().join("list.txt"), b"\xff\xfe not utf8\n")
        .expect("manifest with non-UTF-8 bytes should be written");

    let error = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
        .expect_err("non-UTF-8 manifest bytes must fail");

    assert_eq!(
        error,
        FsError::InvalidPath {
            path: "list.txt".to_owned(),
            reason: "manifest_not_utf8",
        }
    );
}

#[test]
fn audit_manifest_rejects_a_directory_manifest_path() {
    let root = TempDir::new().expect("tree should be created");
    fs::create_dir(root.path().join("list.txt")).expect("directory manifest should be created");

    let error = read_audit_tree_from_manifest(&root.path().join("list.txt"), default_limits())
        .expect_err("a directory in place of the manifest must fail");

    assert_eq!(
        error,
        FsError::UnsupportedFileType {
            path: "list.txt".to_owned(),
        }
    );
}

fn directory_entries(path: &Path) -> Vec<String> {
    let mut entries = fs::read_dir(path)
        .expect("directory should be readable")
        .map(|entry| {
            entry
                .expect("entry should be readable")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}
