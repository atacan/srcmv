//! Bounded, deterministic whole-tree ingestion for the independent
//! conservation audit (`conservation-audit-v1`).
//!
//! This module implements the ingestion model of the conservation-audit plan
//! §6.3 and §7: [`read_audit_tree`] recursively reads a directory tree and
//! [`read_audit_tree_from_manifest`] reads the files named by a `@MANIFEST`.
//! Both forms share one internal ingestion implementation so ordering,
//! stable-read, skip, and limit semantics cannot drift between them.
//!
//! # Skip-versus-error boundary (plan §7)
//!
//! * Per-file instability becomes a **skip** recorded as [`SkippedInput`]:
//!   a file that vanishes mid-walk ([`SkipReason::Vanished`]) or is swapped
//!   for a different file type mid-acquisition, failing stable-read
//!   verification across all three attempts
//!   ([`SkipReason::UnstableReads`]). A partially read tree cannot support a
//!   `pass`; mapping skips to the `unreadable_input_skipped` inconclusive
//!   reason happens downstream in later phases.
//! * Everything else is a **hard error** through the standard [`FsError`]
//!   envelope: missing/unreadable root or manifest, canonicalization failure,
//!   symbolic links including symlinked intermediate components of manifest
//!   entries ([`FsError::SymlinkNotAllowed`]), special file types
//!   ([`FsError::UnsupportedFileType`]), resource-limit violations
//!   ([`FsError::ResourceLimitExceeded`]), and manifest problems including
//!   missing entries ([`FsError::PathNotFound`]) because those are knowable
//!   before any analysis starts.
//!
//! # Error-variant mappings for new situations
//!
//! No new [`FsError`] variants were introduced; the existing vocabulary maps
//! onto every audit failure:
//!
//! * Missing or unreadable root/manifest → [`FsError::Io`] (the CLI maps this
//!   to exit 8 / `IO_ERROR`, matching plan §12).
//! * Root that is not a directory → [`FsError::WorkspaceRootNotDirectory`].
//! * Manifest that exists but is not a regular file →
//!   [`FsError::UnsupportedFileType`] carrying the manifest's final path
//!   component as the safest available spelling.
//! * Manifest bytes that are not UTF-8 → [`FsError::InvalidPath`] with reason
//!   `"manifest_not_utf8"`; per-line syntax problems surface verbatim from the
//!   shared relative-path validator.
//! * Oversized manifests or entry counts, per-file bytes, tree bytes, path
//!   lengths, and file counts → [`FsError::ResourceLimitExceeded`] with stable
//!   resource names (`manifest_bytes`, `manifest_entries`,
//!   `audit_tree_file_bytes`, `audit_tree_total_bytes`, `path_bytes`,
//!   `audit_tree_files`); resource names emitted by the reused snapshot
//!   reader are translated onto these audit-tree spellings at the call
//!   boundary.
//! * A directory entry whose name is not valid UTF-8 → [`FsError::InvalidPath`]
//!   with reason `"entry_not_utf8"` (manifest lines are UTF-8 by definition).
//! * A directory observed during enumeration that disappears before it can be
//!   listed becomes a [`SkipReason::Vanished`] skip like a vanished file,
//!   because that too is mid-walk instability of otherwise-valid input.
//!
//! # Manifest duplicates
//!
//! Exact duplicate lines are deduplicated silently preserving first-occurrence
//! order, mirroring the repeated-spelling tolerance of
//! [`crate::Workspace::inspect`]; distinct case spellings stay distinct paths
//! because case sensitivity follows the filesystem's spelling without folding
//! (plan §7).
//!
//! # Limits discipline
//!
//! All limits use checked arithmetic charged before allocation or I/O,
//! mirroring [`crate::SnapshotLimits`]. Trusted user configuration may lower
//! limits but never raise them; trusted test constructors may pass arbitrary
//! values including `u64::MAX`, exactly like snapshot boundary tests.
//!
//! Directory enumeration is deliberately uncharged by design because the
//! plan §10 table defines no directory-count row; depth is bounded by the
//! 4,096-byte path limit, and the explicit work stack prevents recursion
//! blowups.
//!
//! `AuditTreeLimits::new(max_files = 20_000)` deliberately exceeds
//! `MAX_SNAPSHOT_IDENTITIES = 1_000`: edit batches touch explicit paths while
//! an audit must admit whole checkouts, and the 1 GiB per-tree byte cap
//! remains the primary bound (plan §7). Line indexes are intentionally **not**
//! built during ingestion; they are constructed lazily downstream under the
//! existing per-call caps (plan §7).
//!
//! # Deterministic fault seam
//!
//! Integration tests cannot race a real filesystem reliably, so
//! [`audit_tree_faults`] provides a `#[doc(hidden)]` thread-local seam that
//! deterministically forces vanished-mid-walk, unstable-read, and
//! mid-acquisition type-flip outcomes. It
//! mirrors the crate's debug-only failpoint precedent and is inert in release
//! builds.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use srcmv_core::{Sha256Digest, WorkspaceRelativePath};

use crate::{
    FsError, LEGACY_CONTROL_NAME, MAX_ACQUISITION_ATTEMPTS, MAX_PATH_BYTES,
    MAX_SNAPSHOT_FILE_BYTES, MAX_TOTAL_SNAPSHOT_BYTES, MetadataSignature, checked_add,
    enforce_limit, ensure_supported_platform, io_error, metadata_identity, parse_relative_path,
    read_and_hash_bounded,
};

/// Default maximum number of regular files ingested from one audit tree.
const DEFAULT_AUDIT_TREE_FILES: u64 = 20_000;
/// Default maximum number of entries in one audit manifest.
const DEFAULT_MANIFEST_ENTRIES: u64 = 20_000;
/// Default maximum encoded size of one audit manifest.
const DEFAULT_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

/// Root-level control-directory names skipped in directory mode only.
///
/// These are tooling metadata, not auditable content: Git's `.git`, srcmv's
/// `.srcmv` control tree, and the legacy `.codesplice` control tree. Exactly
/// these three names are skipped, and only at the root; nested directories
/// with these names and every other dotfile are audited content.
const ROOT_SKIP_NAMES: [&str; 3] = [".git", ".srcmv", LEGACY_CONTROL_NAME];

/// Lowerable limits for one conservation-audit tree ingestion.
///
/// Fields are private by house convention; construct with
/// [`AuditTreeLimits::new`] for trusted lower-limit configuration and boundary
/// tests, or use [`AuditTreeLimits::default`] for release values (20,000
/// files, 256 MiB per file, 1 GiB per tree, 4,096 path bytes, manifests of
/// 20,000 entries / 4 MiB).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuditTreeLimits {
    max_files: u64,
    file_bytes: u64,
    total_bytes: u64,
    path_bytes: u64,
    manifest_entries: u64,
    manifest_bytes: u64,
}

impl AuditTreeLimits {
    /// Creates a limit set for trusted lower-limit configuration and boundary tests.
    ///
    /// Test constructors may pass arbitrary values including `u64::MAX`;
    /// user-facing configuration must lower, never raise, release defaults.
    #[must_use]
    pub const fn new(
        max_files: u64,
        file_bytes: u64,
        total_bytes: u64,
        path_bytes: u64,
        manifest_entries: u64,
        manifest_bytes: u64,
    ) -> Self {
        Self {
            max_files,
            file_bytes,
            total_bytes,
            path_bytes,
            manifest_entries,
            manifest_bytes,
        }
    }
}

impl Default for AuditTreeLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_AUDIT_TREE_FILES,
            MAX_SNAPSHOT_FILE_BYTES,
            MAX_TOTAL_SNAPSHOT_BYTES,
            MAX_PATH_BYTES,
            DEFAULT_MANIFEST_ENTRIES,
            DEFAULT_MANIFEST_BYTES,
        )
    }
}

/// One regular file ingested from an audit tree, with exact bytes and digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditTreeFile {
    /// Normalized workspace-relative path spelled by the walk or manifest.
    pub path: WorkspaceRelativePath,
    /// Exact immutable file contents.
    pub bytes: Arc<[u8]>,
    /// SHA-256 digest computed during the bounded stable read.
    pub digest: Sha256Digest,
}

/// Why one observed input was skipped instead of ingested.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    /// The listed entry disappeared before or while it was read.
    Vanished,
    /// The entry remained demonstrably unstable across all three read attempts.
    UnstableReads,
}

/// One file observed but not ingested, with the reason it was skipped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedInput {
    /// Normalized workspace-relative spelling of the skipped entry.
    pub path: WorkspaceRelativePath,
    /// Why the entry produced no [`AuditTreeFile`].
    pub reason: SkipReason,
}

/// Recursively reads one directory tree into bounded audit inputs.
///
/// The root is canonicalized once with the same contract as
/// [`crate::Workspace::open`]: the root may be reached through a symbolic
/// link, but every entry beneath the canonical root must be a regular file or
/// a real directory. Traversal is deterministic (sorted byte order,
/// depth-first) and exactly the three root-level control names (`.git`,
/// `.srcmv`, `.codesplice`) are skipped; nested directories with these names
/// and all other dotfiles are audited content. Empty files are valid; empty
/// directories contribute nothing. Symbolic links and special files anywhere
/// beneath the root are hard errors. Entries vanished mid-walk become
/// [`SkippedInput`] records.
///
/// Returned files are sorted by normalized path bytes; skip records are
/// sorted alongside.
///
/// # Errors
///
/// Returns a typed filesystem error for platform, canonicalization, type,
/// limit, or I/O failures per the module-level boundary documentation.
pub fn read_audit_tree(
    root: &Path,
    limits: AuditTreeLimits,
) -> Result<(Vec<AuditTreeFile>, Vec<SkippedInput>), FsError> {
    ensure_supported_platform()?;
    let canonical_root =
        fs::canonicalize(root).map_err(|error| io_error("canonicalize_workspace", None, error))?;
    let metadata = fs::symlink_metadata(&canonical_root)
        .map_err(|error| io_error("inspect_workspace", None, error))?;
    if !metadata.is_dir() {
        return Err(FsError::WorkspaceRootNotDirectory);
    }

    let mut ingester = TreeIngester::new(&canonical_root, limits, false);
    walk_directory(&canonical_root, String::new(), &mut ingester)?;
    finish(ingester)
}

/// Reads exactly the files named by a `@MANIFEST` into bounded audit inputs.
///
/// The manifest must be a UTF-8 text file holding one normalized relative path
/// per line; `#` comments and blank lines are allowed and a single trailing
/// carriage return is tolerated. Every line is validated by the existing
/// relative-path rules (rejecting absolute spellings, `..` escapes, NUL
/// bytes, empty components, over-long paths, and reserved first components)
/// and then resolved against the manifest's parent directory. Missing targets
/// are hard errors because they are knowable before any analysis begins.
/// Exact duplicate lines are deduplicated silently preserving first-occurrence
/// order; distinct case spellings stay distinct entries.
///
/// Returned files are sorted by normalized path bytes; skip records are
/// sorted alongside.
///
/// # Errors
///
/// Returns a typed filesystem error for unreadable or malformed manifests,
/// invalid line spellings, missing entries, unstable reads recorded as skips,
/// and resource-limit violations per the module-level boundary documentation.
pub fn read_audit_tree_from_manifest(
    manifest_path: &Path,
    limits: AuditTreeLimits,
) -> Result<(Vec<AuditTreeFile>, Vec<SkippedInput>), FsError> {
    ensure_supported_platform()?;
    let canonical_manifest = fs::canonicalize(manifest_path)
        .map_err(|error| io_error("canonicalize_manifest", None, error))?;
    let metadata = fs::symlink_metadata(&canonical_manifest)
        .map_err(|error| io_error("inspect_manifest", None, error))?;
    if !metadata.is_file() {
        return Err(FsError::UnsupportedFileType {
            path: safe_manifest_spelling(manifest_path),
        });
    }
    enforce_limit("manifest_bytes", metadata.len(), limits.manifest_bytes)?;
    // Bounded read chosen over the post-read re-stat alternative: the
    // `take(manifest_bytes + 1)` sentinel caps allocation even when the file
    // grows past the pre-open size check (TOCTOU), and the post-read limit
    // charge converts that growth into `manifest_bytes` rejection.
    let mut raw = Vec::new();
    File::open(&canonical_manifest)
        .map_err(|error| io_error("open_manifest", None, error))?
        .take(limits.manifest_bytes.saturating_add(1))
        .read_to_end(&mut raw)
        .map_err(|error| io_error("read_manifest", None, error))?;
    enforce_limit(
        "manifest_bytes",
        u64::try_from(raw.len()).unwrap_or(u64::MAX),
        limits.manifest_bytes,
    )?;
    let text = String::from_utf8(raw).map_err(|_| FsError::InvalidPath {
        path: safe_manifest_spelling(manifest_path),
        reason: "manifest_not_utf8",
    })?;

    let base = canonical_manifest
        .parent()
        .ok_or(FsError::InternalInvariant {
            invariant: "canonical_manifest_has_parent",
        })?
        .to_path_buf();
    let entries = parse_manifest_entries(&text, limits)?;

    let mut ingester = TreeIngester::new(&base, limits, true);
    for entry in &entries {
        ingester.ingest(entry)?;
    }
    finish(ingester)
}

/// Deterministic fault injection seam for integration tests.
///
/// Faults affect only the thread that installs them, so parallel tests cannot
/// interfere. Like the crate's environment failpoints this seam is compiled
/// out of release builds, where installation is a no-op and no fault ever
/// fires.
#[doc(hidden)]
pub mod audit_tree_faults {
    use super::RefCell;

    /// Relative-path faults installed for the current thread.
    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    pub struct Faults {
        /// Paths reported as vanished regardless of their on-disk state.
        pub vanish: Vec<String>,
        /// Paths forced to fail stable-read verification on every attempt.
        pub unstable: Vec<String>,
        /// Paths forced to report a non-regular file type after opening,
        /// simulating a delete-and-recreate-with-different-type race.
        pub type_flip: Vec<String>,
    }

    #[cfg(debug_assertions)]
    thread_local! {
        static ACTIVE: RefCell<Option<Faults>> = const { RefCell::new(None) };
    }

    /// Installs faults for the current thread, replacing any previous set.
    pub fn install(faults: Faults) {
        #[cfg(debug_assertions)]
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(faults));
        #[cfg(not(debug_assertions))]
        let _ = faults;
    }

    /// Removes all faults from the current thread.
    pub fn clear() {
        #[cfg(debug_assertions)]
        ACTIVE.with(|slot| *slot.borrow_mut() = None);
    }

    /// Reports whether the path is configured to appear vanished.
    pub(crate) fn forces_vanish(path: &str) -> bool {
        #[cfg(debug_assertions)]
        {
            ACTIVE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|faults| faults.vanish.iter().any(|name| name == path))
            })
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = path;
            false
        }
    }

    /// Reports whether the path is configured to fail stable verification.
    pub(crate) fn forces_unstable(path: &str) -> bool {
        #[cfg(debug_assertions)]
        {
            ACTIVE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|faults| faults.unstable.iter().any(|name| name == path))
            })
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = path;
            false
        }
    }

    /// Reports whether the path is configured to appear as a non-regular file
    /// after opening.
    pub(crate) fn forces_type_flip(path: &str) -> bool {
        #[cfg(debug_assertions)]
        {
            ACTIVE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|faults| faults.type_flip.iter().any(|name| name == path))
            })
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = path;
            false
        }
    }
}

/// Outcome of one bounded stable-read attempt for one candidate file.
enum AttemptOutcome {
    /// The candidate was read and verified stable.
    Stable(AuditTreeFile),
    /// The candidate was absent at a terminal checkpoint (the attempt-start
    /// stat or the open itself).
    Vanished,
    /// The candidate changed under us and another attempt may succeed.
    Unstable,
}

/// Shared ingestion state used identically by directory and manifest modes.
struct TreeIngester<'a> {
    base: &'a Path,
    limits: AuditTreeLimits,
    strict_missing: bool,
    committed_files: u64,
    committed_bytes: u64,
    files: Vec<AuditTreeFile>,
    skips: Vec<SkippedInput>,
}

impl<'a> TreeIngester<'a> {
    fn new(base: &'a Path, limits: AuditTreeLimits, strict_missing: bool) -> Self {
        Self {
            base,
            limits,
            strict_missing,
            committed_files: 0,
            committed_bytes: 0,
            files: Vec::new(),
            skips: Vec::new(),
        }
    }

    /// Ingests one candidate path, converting per-file instability into skips.
    ///
    /// The file-count limit is projected and enforced before any I/O for this
    /// candidate; in manifest mode the entry's intermediate components are
    /// then validated once per entry (policy checks never consume the retry
    /// budget); per-file and aggregate byte limits are enforced from the
    /// opening metadata before the bounded read allocates content. Counts and
    /// bytes commit only after a verified stable read, so skipped candidates
    /// consume no budget.
    fn ingest(&mut self, path: &WorkspaceRelativePath) -> Result<(), FsError> {
        if audit_tree_faults::forces_vanish(&path.value) {
            self.record_skip(path.clone(), SkipReason::Vanished);
            return Ok(());
        }
        enforce_limit(
            "audit_tree_files",
            checked_add(self.committed_files, 1, "audit_tree_files")?,
            self.limits.max_files,
        )?;
        if self.strict_missing {
            ensure_real_intermediate_components(self.base, path)?;
        }

        for attempt in 0..MAX_ACQUISITION_ATTEMPTS {
            match self.attempt_once(path, attempt)? {
                AttemptOutcome::Stable(file) => {
                    self.committed_files += 1;
                    self.committed_bytes = self
                        .committed_bytes
                        .saturating_add(u64::try_from(file.bytes.len()).unwrap_or(u64::MAX));
                    self.files.push(file);
                    return Ok(());
                }
                AttemptOutcome::Vanished => {
                    if self.strict_missing {
                        return Err(FsError::PathNotFound {
                            path: path.value.clone(),
                        });
                    }
                    self.record_skip(path.clone(), SkipReason::Vanished);
                    return Ok(());
                }
                AttemptOutcome::Unstable if attempt + 1 < MAX_ACQUISITION_ATTEMPTS => {}
                AttemptOutcome::Unstable => {
                    self.record_skip(path.clone(), SkipReason::UnstableReads);
                    return Ok(());
                }
            }
        }
        Err(FsError::InternalInvariant {
            invariant: "bounded_audit_ingestion_loop_returns",
        })
    }

    fn record_skip(&mut self, path: WorkspaceRelativePath, reason: SkipReason) {
        self.skips.push(SkippedInput { path, reason });
    }

    /// Runs one bounded stable-read attempt for one candidate.
    ///
    /// Two classifications are pinned here. First, NotFound is terminal from
    /// either checkpoint — the attempt-start stat and [`File::open`] both
    /// yield [`AttemptOutcome::Vanished`] so a candidate observed missing at
    /// one checkpoint is classified identically at the other. Second, a
    /// non-regular observation is a policy verdict only on the first attempt
    /// of a manifest entry (manifest entries receive no listing-time
    /// classification, unlike directory walks where the walker already
    /// rejected stable special types); on any later attempt it is
    /// mid-acquisition instability and yields [`AttemptOutcome::Unstable`],
    /// leaving the remaining attempts to settle the outcome.
    fn attempt_once(
        &mut self,
        path: &WorkspaceRelativePath,
        attempt: usize,
    ) -> Result<AttemptOutcome, FsError> {
        let full_path = self.base.join(&path.value);
        let entry_before = match fs::symlink_metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AttemptOutcome::Vanished);
            }
            Err(error) => {
                return Err(io_error("inspect_audit_entry", Some(&path.value), error));
            }
        };
        if entry_before.file_type().is_symlink() {
            return Err(FsError::SymlinkNotAllowed {
                path: path.value.clone(),
            });
        }
        if !entry_before.is_file() {
            if self.strict_missing && attempt == 0 {
                return Err(FsError::UnsupportedFileType {
                    path: path.value.clone(),
                });
            }
            return Ok(AttemptOutcome::Unstable);
        }
        let parent_path = full_path.parent().ok_or(FsError::InternalInvariant {
            invariant: "audit_candidate_has_parent",
        })?;
        let parent_before = match fs::symlink_metadata(parent_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AttemptOutcome::Unstable);
            }
            Err(error) => {
                return Err(io_error("inspect_audit_parent", Some(&path.value), error));
            }
        };
        if !parent_before.is_dir() {
            return Ok(AttemptOutcome::Unstable);
        }
        let parent_identity = metadata_identity(&parent_before);

        let mut file = match File::open(&full_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AttemptOutcome::Vanished);
            }
            Err(error) => {
                return Err(io_error("open_audit_file", Some(&path.value), error));
            }
        };
        let metadata_before = file
            .metadata()
            .map_err(|error| io_error("inspect_open_audit", Some(&path.value), error))?;
        // The fault seam drives this branch deterministically to simulate a
        // delete-and-recreate-with-different-type race between checkpoints.
        if !metadata_before.is_file() || audit_tree_faults::forces_type_flip(&path.value) {
            return Ok(AttemptOutcome::Unstable);
        }
        let signature_before = MetadataSignature::from(&metadata_before);

        enforce_limit(
            "audit_tree_file_bytes",
            metadata_before.len(),
            self.limits.file_bytes,
        )?;
        enforce_limit(
            "audit_tree_total_bytes",
            checked_add(
                self.committed_bytes,
                metadata_before.len(),
                "audit_tree_total_bytes",
            )?,
            self.limits.total_bytes,
        )?;
        let (bytes, digest) = read_and_hash_bounded(
            &mut file,
            self.limits.file_bytes,
            self.committed_bytes,
            self.limits.total_bytes,
            path,
        )
        .map_err(translate_reader_resource_names)?;
        if audit_tree_faults::forces_unstable(&path.value) {
            return Ok(AttemptOutcome::Unstable);
        }

        let metadata_after = file
            .metadata()
            .map_err(|error| io_error("reinspect_open_audit", Some(&path.value), error))?;
        let entry_after = match fs::symlink_metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AttemptOutcome::Unstable);
            }
            Err(error) => {
                return Err(io_error("confirm_audit_entry", Some(&path.value), error));
            }
        };
        let parent_after = match fs::symlink_metadata(parent_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AttemptOutcome::Unstable);
            }
            Err(error) => {
                return Err(io_error("confirm_audit_parent", Some(&path.value), error));
            }
        };
        let stable = MetadataSignature::from(&metadata_after) == signature_before
            && entry_after.is_file()
            && metadata_identity(&entry_after) == signature_before.identity
            && parent_after.is_dir()
            && metadata_identity(&parent_after) == parent_identity
            && metadata_after.len() == u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if !stable {
            return Ok(AttemptOutcome::Unstable);
        }

        Ok(AttemptOutcome::Stable(AuditTreeFile {
            path: path.clone(),
            bytes: Arc::from(bytes),
            digest,
        }))
    }
}

/// Rejects manifest entries whose traversal crosses a symbolic link, once per
/// entry before acquisition begins.
///
/// Chosen implementation of the §7 mechanical symlink boundary (over an
/// immediate-parent-only check inside each attempt): every intermediate
/// component is validated one-by-one exactly like the workspace relative-path
/// validator — each existing ancestor must be a real directory, and a symlink
/// at any depth is [`FsError::SymlinkNotAllowed`] rather than a retry-budget
/// skip. A missing ancestor defers to the candidate's own vanished handling,
/// which stays a [`FsError::PathNotFound`] hard error in manifest mode.
fn ensure_real_intermediate_components(
    base: &Path,
    path: &WorkspaceRelativePath,
) -> Result<(), FsError> {
    let Some(prefix) = path.value.rsplit_once('/').map(|(prefix, _)| prefix) else {
        return Ok(());
    };
    let mut current = base.to_path_buf();
    for component in prefix.split('/') {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(io_error(
                    "inspect_manifest_parent",
                    Some(&path.value),
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(FsError::SymlinkNotAllowed {
                path: path.value.clone(),
            });
        }
        if !metadata.is_dir() {
            return Err(FsError::UnsupportedFileType {
                path: path.value.clone(),
            });
        }
    }
    Ok(())
}

/// Translates snapshot-scoped resource names emitted by the shared bounded
/// reader onto the audit-tree names promised by the module documentation.
///
/// Only the stable `resource` spelling changes when the reused
/// `read_and_hash_bounded` trips its internal guards mid-read; variant,
/// observed usage, and limit pass through untouched.
fn translate_reader_resource_names(error: FsError) -> FsError {
    match error {
        FsError::ResourceLimitExceeded {
            resource,
            actual,
            limit,
        } => FsError::ResourceLimitExceeded {
            resource: match resource {
                "snapshot_file_bytes" => "audit_tree_file_bytes",
                "snapshot_bytes" => "audit_tree_total_bytes",
                other => other,
            },
            actual,
            limit,
        },
        other => other,
    }
}

/// Walks one tree depth-first in strict pre-order over ascending byte-sorted
/// entry names.
///
/// Work items are pushed in reverse so pops yield exact ascending pre-order;
/// the explicit stack bounds traversal memory without recursion. Entries that
/// disappear after being listed become [`SkipReason::Vanished`] skips; policy
/// violations stay hard errors.
fn walk_directory(
    root: &Path,
    root_relative: String,
    ingester: &mut TreeIngester<'_>,
) -> Result<(), FsError> {
    enum Work {
        /// Lists `dir_path`'s entries and queues them for classification.
        Enter(PathBuf, String),
        /// Classifies one queued entry by its current on-disk state.
        Classify(PathBuf, String),
    }
    let mut stack: Vec<Work> = vec![Work::Enter(root.to_path_buf(), root_relative)];
    while let Some(item) = stack.pop() {
        match item {
            Work::Enter(dir_path, dir_relative) => {
                let mut names = list_sorted_names(&dir_path, &dir_relative)?;
                names.reverse();
                let root_level = dir_relative.is_empty();
                for name in names {
                    let Some(spelling) = name.to_str() else {
                        return Err(FsError::InvalidPath {
                            path: name.to_string_lossy().into_owned(),
                            reason: "entry_not_utf8",
                        });
                    };
                    let child_relative = if root_level {
                        spelling.to_owned()
                    } else {
                        format!("{}/{}", dir_relative, spelling)
                    };
                    if root_level && ROOT_SKIP_NAMES.contains(&child_relative.as_str()) {
                        continue;
                    }
                    stack.push(Work::Classify(dir_path.join(&name), child_relative));
                }
            }
            Work::Classify(child_path, child_relative) => {
                enforce_limit(
                    "path_bytes",
                    u64::try_from(child_relative.len()).unwrap_or(u64::MAX),
                    ingester.limits.path_bytes,
                )?;
                let metadata = match fs::symlink_metadata(&child_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        ingester.skips.push(SkippedInput {
                            path: WorkspaceRelativePath {
                                value: child_relative,
                            },
                            reason: SkipReason::Vanished,
                        });
                        continue;
                    }
                    Err(error) => {
                        return Err(io_error(
                            "inspect_audit_entry",
                            Some(child_relative.as_str()),
                            error,
                        ));
                    }
                };
                if metadata.is_symlink() {
                    return Err(FsError::SymlinkNotAllowed {
                        path: child_relative,
                    });
                }
                if metadata.is_dir() {
                    stack.push(Work::Enter(child_path, child_relative));
                    continue;
                }
                if !metadata.is_file() {
                    return Err(FsError::UnsupportedFileType {
                        path: child_relative,
                    });
                }
                ingester.ingest(&WorkspaceRelativePath {
                    value: child_relative,
                })?;
            }
        }
    }
    Ok(())
}

/// Lists one directory's entry names sorted by raw byte order.
fn list_sorted_names(directory: &Path, relative: &str) -> Result<Vec<OsString>, FsError> {
    let read_error = |error: io::Error| io_error("list_audit_directory", Some(relative), error);
    let mut names = match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|item| item.file_name()).map_err(read_error))
            .collect::<Result<Vec<_>, FsError>>()?,
        Err(error) => return Err(read_error(error)),
    };
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

/// Parses validated, deduplicated manifest entries in first-occurrence order.
///
/// Blank lines and `#` comments are ignored; one trailing carriage return per
/// line is tolerated; surrounding whitespace is otherwise significant. The
/// unique-entry limit is charged with checked arithmetic as each new distinct
/// validated spelling is accepted.
fn parse_manifest_entries(
    text: &str,
    limits: AuditTreeLimits,
) -> Result<Vec<WorkspaceRelativePath>, FsError> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let normalized = parse_relative_path(line, limits.path_bytes)?;
        if seen.insert(normalized.value.clone()) {
            enforce_limit(
                "manifest_entries",
                checked_add(
                    u64::try_from(entries.len()).unwrap_or(u64::MAX),
                    1,
                    "manifest_entries",
                )?,
                limits.manifest_entries,
            )?;
            entries.push(normalized);
        }
    }
    Ok(entries)
}

/// Produces the safest short spelling of a manifest path for error context.
fn safe_manifest_spelling(manifest_path: &Path) -> String {
    manifest_path.file_name().map_or_else(
        || "manifest".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Sorts collected files and skips into canonical path-byte order.
fn finish(
    mut ingester: TreeIngester<'_>,
) -> Result<(Vec<AuditTreeFile>, Vec<SkippedInput>), FsError> {
    ingester
        .files
        .sort_by(|left, right| left.path.value.as_bytes().cmp(right.path.value.as_bytes()));
    ingester
        .skips
        .sort_by(|left, right| left.path.value.as_bytes().cmp(right.path.value.as_bytes()));
    Ok((ingester.files, ingester.skips))
}

#[cfg(test)]
mod audit_tree_tests {
    use super::*;

    fn limits(entries: u64) -> AuditTreeLimits {
        AuditTreeLimits::new(1_000, u64::MAX, u64::MAX, 4_096, entries, u64::MAX)
    }

    fn values(entries: &[WorkspaceRelativePath]) -> Vec<String> {
        entries.iter().map(|entry| entry.value.clone()).collect()
    }

    #[test]
    fn parse_should_ignore_comments_blank_lines_and_carriage_returns() {
        let text = "# header comment\r\nalpha.txt\r\n\r\n   \n# another\nbeta/gamma.txt\r\n";
        let parsed = parse_manifest_entries(text, limits(10)).expect("entries should parse");

        assert_eq!(values(&parsed), vec!["alpha.txt", "beta/gamma.txt"]);
    }

    #[test]
    fn parse_should_deduplicate_exact_spellings_preserving_first_occurrence_order() {
        let text = "zeta.txt\none.txt\nzeta.txt\none.txt\ntwo.txt\n";
        let parsed = parse_manifest_entries(text, limits(10)).expect("entries should parse");

        assert_eq!(values(&parsed), vec!["zeta.txt", "one.txt", "two.txt"]);
    }

    #[test]
    fn parse_should_keep_case_distinct_spellings_as_separate_entries() {
        let text = "Alpha.txt\nalpha.txt\n";
        let parsed = parse_manifest_entries(text, limits(10)).expect("entries should parse");

        assert_eq!(values(&parsed), vec!["Alpha.txt", "alpha.txt"]);
    }

    #[test]
    fn parse_should_reject_escaping_absolute_and_reserved_spellings() {
        for line in [
            "../escape.txt",
            "/absolute.txt",
            "double//slash.txt",
            ".srcmv/control",
            "trailing/",
            "./current.txt",
        ] {
            let error = parse_manifest_entries(line, limits(10))
                .expect_err("invalid spelling should be rejected");
            assert!(matches!(error, FsError::InvalidPath { .. }), "{line}");
        }
    }

    #[test]
    fn parse_should_enforce_unique_entry_count_at_and_above_the_limit() {
        let text = "one.txt\ntwo.txt\nthree.txt\n";

        assert_eq!(
            parse_manifest_entries(text, limits(3))
                .expect("usage below the limit should pass")
                .len(),
            3
        );
        assert!(matches!(
            parse_manifest_entries(text, limits(2)),
            Err(FsError::ResourceLimitExceeded {
                resource: "manifest_entries",
                actual: 3,
                limit: 2,
            })
        ));
    }
}
