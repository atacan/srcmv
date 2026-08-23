#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! Workspace inspection, immutable snapshots, and persistent transaction control.
//!
//! This crate owns canonical workspace resolution, strict relative-path walks,
//! POSIX physical identities, bounded stable reads, snapshot accounting, and the
//! persistent transaction control tree.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use srcmv_core::{
    AbsentPathSnapshot, CoreError, FileIdentity, FileSnapshot, LineIndex, Sha256Digest,
    SnapshotFileId, WorkspaceRelativePath, WorkspaceSnapshot,
};

mod audit_tree;
mod control;
mod journal;
mod recovery_classifier;
mod transaction;

pub use audit_tree::audit_tree_faults;
pub use audit_tree::{
    AuditTreeFile, AuditTreeLimits, SkipReason, SkippedInput, read_audit_tree,
    read_audit_tree_from_manifest,
};
pub use control::LEGACY_CONTROL_NAME;
pub use control::{
    ControlObservation, DiagnosticLock, MutationLock, RecoveryEntry, RecoveryEntryKind,
    TransactionDirectory,
};
pub use journal::{
    CandidateKind, CandidateState, CommitKind, CommitState, GlobalState, Manifest, ManifestInput,
    ManifestSegment, ManifestTarget, MetadataPolicy, PersistedIdentity, RollbackKind,
    RollbackState, StateSnapshot, TargetState, TransactionJournal, TransactionLimits,
    decode_manifest_record, decode_manifest_record_with_limits, decode_state_record,
    decode_state_record_with_limits, encode_manifest_record, encode_manifest_record_with_limits,
    encode_state_record, encode_state_record_with_limits, validate_state_transition,
};
pub use recovery_classifier::{
    LocationObservation, RecoveryDisposition, SyntheticTargetObservation, classify_recovery,
};
pub use transaction::{CommitOutcome, QualifiedFilesystem, RecoveryOutcome};

/// Maximum bytes in one immutable file snapshot.
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Maximum aggregate bytes in one workspace snapshot.
pub const MAX_TOTAL_SNAPSHOT_BYTES: u64 = 1024 * 1024 * 1024;
/// Maximum aggregate lines in one workspace snapshot.
pub const MAX_TOTAL_LINE_COUNT: u64 = 5_000_000;
/// Maximum aggregate compact line-index bytes in one workspace snapshot.
pub const MAX_LINE_INDEX_MEMORY: u64 = 256 * 1024 * 1024;
/// Maximum distinct path identities inspected in one acquisition.
pub const MAX_SNAPSHOT_IDENTITIES: u64 = 1_000;
/// Maximum encoded bytes in one normalized relative path.
pub const MAX_PATH_BYTES: u64 = 4_096;

const MAX_ACQUISITION_ATTEMPTS: usize = 3;

/// Lowerable limits for read-only workspace inspection and snapshot acquisition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotLimits {
    path_bytes: u64,
    identities: u64,
    file_bytes: u64,
    total_bytes: u64,
    total_lines: u64,
    line_index_memory: u64,
}

impl SnapshotLimits {
    /// Creates a limit set for trusted lower-limit configuration and boundary tests.
    #[must_use]
    pub const fn new(
        path_bytes: u64,
        identities: u64,
        file_bytes: u64,
        total_bytes: u64,
        total_lines: u64,
        line_index_memory: u64,
    ) -> Self {
        Self {
            path_bytes,
            identities,
            file_bytes,
            total_bytes,
            total_lines,
            line_index_memory,
        }
    }
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self::new(
            MAX_PATH_BYTES,
            MAX_SNAPSHOT_IDENTITIES,
            MAX_SNAPSHOT_FILE_BYTES,
            MAX_TOTAL_SNAPSHOT_BYTES,
            MAX_TOTAL_LINE_COUNT,
            MAX_LINE_INDEX_MEMORY,
        )
    }
}

/// The required initial state for one immutable snapshot path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredPathState {
    /// The path must be a regular file with this digest.
    Existing(Sha256Digest),
    /// The final component must not exist beneath a valid existing parent.
    Absent,
}

/// One normalized path and precondition requested for snapshot acquisition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRequirement {
    /// Normalized workspace-relative path.
    pub path: WorkspaceRelativePath,
    /// Required existing digest or absence.
    pub state: RequiredPathState,
}

/// Read-only observation returned for one inspected path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathInspection {
    /// Normalized workspace-relative path.
    pub path: WorkspaceRelativePath,
    /// Existing regular-file details or validated absence.
    pub state: InspectedState,
}

/// The observed state of an inspected path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InspectedState {
    /// An immutable regular-file observation.
    Existing {
        /// Content digest.
        digest: Sha256Digest,
        /// Exact byte length.
        byte_length: u64,
        /// Number of logical lines.
        line_count: u64,
        /// Opaque hash of the POSIX physical identity.
        identity_hash: Sha256Digest,
    },
    /// An absent final component beneath a validated parent.
    Absent,
}

/// An opened, canonical workspace root retained with its physical identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    canonical_root: PathBuf,
    identity: FileIdentity,
}

impl Workspace {
    /// Resolves a workspace root once to an absolute canonical directory.
    ///
    /// A symbolic link used only to spell the root is canonicalized as permitted
    /// by the v0.1 contract. Symbolic links beneath that canonical root are rejected.
    ///
    /// # Errors
    ///
    /// Returns a typed filesystem error if the platform is unsupported, the root
    /// cannot be canonicalized, or the canonical object is not a directory.
    pub fn open(path: &Path) -> Result<Self, FsError> {
        ensure_supported_platform()?;
        let canonical_root = fs::canonicalize(path)
            .map_err(|error| io_error("canonicalize_workspace", None, error))?;
        let metadata = fs::symlink_metadata(&canonical_root)
            .map_err(|error| io_error("inspect_workspace", None, error))?;
        if !metadata.is_dir() {
            return Err(FsError::WorkspaceRootNotDirectory);
        }
        Ok(Self {
            canonical_root,
            identity: metadata_identity(&metadata),
        })
    }

    /// Returns the absolute canonical root selected for this workspace.
    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// Returns the retained physical identity of the canonical root.
    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Returns the opaque protocol hash of the canonical root's physical identity.
    #[must_use]
    pub fn identity_hash(&self) -> Sha256Digest {
        hash_identity(self.identity)
    }

    /// Inspects paths using the same path walk, stable read, index, and limit logic
    /// used for planning snapshots.
    ///
    /// Results preserve request order. Repeated identical spellings reuse one
    /// observation; distinct spellings of one physical file are rejected.
    ///
    /// # Errors
    ///
    /// Returns a path, type, alias, stability, limit, platform, or I/O error when
    /// a trustworthy observation cannot be acquired.
    pub fn inspect(
        &self,
        paths: &[String],
        limits: SnapshotLimits,
    ) -> Result<Vec<PathInspection>, FsError> {
        let mut observations = BTreeMap::new();
        let mut aliases = HashMap::new();
        let mut accounting = SnapshotAccounting::default();

        for path in paths {
            let normalized = parse_relative_path(path, limits.path_bytes)?;
            if observations.contains_key(&normalized.value) {
                continue;
            }
            accounting.charge_identity(limits)?;
            let inspection =
                match self.acquire_path(&normalized, None, limits, &accounting, |_| {})? {
                    AcquiredPath::Existing(file) => {
                        reject_alias(&mut aliases, file.identity, &normalized)?;
                        accounting.charge_file(&file, limits)?;
                        PathInspection {
                            path: normalized.clone(),
                            state: InspectedState::Existing {
                                digest: file.digest,
                                byte_length: u64::try_from(file.bytes.len()).unwrap_or(u64::MAX),
                                line_count: file.line_index.line_count(),
                                identity_hash: hash_identity(file.identity),
                            },
                        }
                    }
                    AcquiredPath::Absent(_) => PathInspection {
                        path: normalized.clone(),
                        state: InspectedState::Absent,
                    },
                };
            observations.insert(normalized.value, inspection);
        }

        paths
            .iter()
            .map(|path| {
                observations
                    .get(path)
                    .cloned()
                    .ok_or(FsError::InternalInvariant {
                        invariant: "inspection_result_for_each_validated_path",
                    })
            })
            .collect()
    }

    /// Acquires existing inputs and required absences into one immutable snapshot.
    ///
    /// Existing files are ordered by normalized UTF-8 path and receive stable
    /// snapshot IDs in that order. Repeated paths must use the same precondition.
    ///
    /// # Errors
    ///
    /// Returns a typed error for incompatible preconditions, stale digests,
    /// aliases, invalid paths, unstable files, special files, or resource excess.
    pub fn acquire_snapshot(
        &self,
        requirements: &[SnapshotRequirement],
        limits: SnapshotLimits,
    ) -> Result<WorkspaceSnapshot, FsError> {
        self.acquire_snapshot_with_hook(requirements, limits, |_, _| {})
    }

    /// Acquires one existing regular file into an immutable snapshot without a
    /// caller-supplied digest precondition.
    ///
    /// The returned file always has snapshot ID zero. The path is validated as a
    /// normalized workspace-relative path, and acquisition uses the same strict
    /// no-symlink walk, bounded stable read, identity capture, digest, line index,
    /// and accounting rules as [`Self::acquire_snapshot`].
    ///
    /// # Errors
    ///
    /// Returns [`FsError::PathNotFound`] when the normalized path is absent, or a
    /// typed path, type, stability, limit, platform, or I/O error when a trustworthy
    /// existing-file snapshot cannot be acquired.
    pub fn acquire_existing_file(
        &self,
        path: &WorkspaceRelativePath,
        limits: SnapshotLimits,
    ) -> Result<FileSnapshot, FsError> {
        let normalized = parse_relative_path(&path.value, limits.path_bytes)?;
        let mut accounting = SnapshotAccounting::default();
        accounting.charge_identity(limits)?;
        match self.acquire_path(&normalized, None, limits, &accounting, |_| {})? {
            AcquiredPath::Existing(mut file) => {
                let mut aliases = HashMap::new();
                reject_alias(&mut aliases, file.identity, &normalized)?;
                accounting.charge_file(&file, limits)?;
                file.id = SnapshotFileId(0);
                Ok(file)
            }
            AcquiredPath::Absent(_) => Err(FsError::PathNotFound {
                path: normalized.value,
            }),
        }
    }

    fn acquire_snapshot_with_hook<F>(
        &self,
        requirements: &[SnapshotRequirement],
        limits: SnapshotLimits,
        mut after_read: F,
    ) -> Result<WorkspaceSnapshot, FsError>
    where
        F: FnMut(usize, &Path),
    {
        let mut unique = BTreeMap::new();
        for requirement in requirements {
            let normalized = parse_relative_path(&requirement.path.value, limits.path_bytes)?;
            match unique.get(&normalized.value) {
                Some(previous) if *previous != requirement.state => {
                    return Err(FsError::IncompatiblePrecondition {
                        path: normalized.value,
                    });
                }
                Some(_) => {}
                None => {
                    unique.insert(normalized.value, requirement.state);
                }
            }
        }
        enforce_limit(
            "snapshot_identities",
            u64::try_from(unique.len()).unwrap_or(u64::MAX),
            limits.identities,
        )?;

        let mut files = Vec::new();
        let mut absent_paths = Vec::new();
        let mut aliases = HashMap::new();
        let mut accounting = SnapshotAccounting::default();

        for (path, required) in unique {
            accounting.charge_identity(limits)?;
            let normalized = WorkspaceRelativePath { value: path };
            match self.acquire_path(
                &normalized,
                Some(required),
                limits,
                &accounting,
                |attempt| {
                    after_read(
                        attempt,
                        self.canonical_root.join(&normalized.value).as_path(),
                    )
                },
            )? {
                AcquiredPath::Existing(mut file) => {
                    reject_alias(&mut aliases, file.identity, &normalized)?;
                    accounting.charge_file(&file, limits)?;
                    file.id = SnapshotFileId(u64::try_from(files.len()).unwrap_or(u64::MAX));
                    files.push(file);
                }
                AcquiredPath::Absent(absent) => absent_paths.push(absent),
            }
        }

        Ok(WorkspaceSnapshot {
            workspace_identity: self.identity,
            files: files.into(),
            absent_paths: absent_paths.into(),
        })
    }

    fn acquire_path<F>(
        &self,
        path: &WorkspaceRelativePath,
        required: Option<RequiredPathState>,
        limits: SnapshotLimits,
        accounting: &SnapshotAccounting,
        mut after_read: F,
    ) -> Result<AcquiredPath, FsError>
    where
        F: FnMut(usize),
    {
        for attempt in 0..MAX_ACQUISITION_ATTEMPTS {
            let validated = self.validate_path(path)?;
            match (&validated.kind, required) {
                (ValidatedKind::Absent, Some(RequiredPathState::Existing(expected))) => {
                    return Err(FsError::PreconditionFailed {
                        path: path.value.clone(),
                        expected: Some(expected),
                        actual: None,
                    });
                }
                (ValidatedKind::Existing(_), Some(RequiredPathState::Absent)) => {
                    return Err(FsError::PreconditionFailed {
                        path: path.value.clone(),
                        expected: None,
                        actual: None,
                    });
                }
                (ValidatedKind::Absent, _) => {
                    return Ok(AcquiredPath::Absent(AbsentPathSnapshot {
                        path: path.clone(),
                        parent_identity: validated.parent_identity,
                        parent_identities: Arc::clone(&validated.parent_identities),
                        basename: validated.basename,
                    }));
                }
                (ValidatedKind::Existing(_), _) => {}
            }

            let expected = match required {
                Some(RequiredPathState::Existing(digest)) => Some(digest),
                None | Some(RequiredPathState::Absent) => None,
            };
            match self.acquire_existing_attempt(
                path,
                &validated,
                limits,
                accounting,
                attempt,
                &mut after_read,
            )? {
                AttemptResult::Stable(file) => {
                    if expected.is_some_and(|expected| expected != file.digest) {
                        return Err(FsError::PreconditionFailed {
                            path: path.value.clone(),
                            expected,
                            actual: Some(file.digest),
                        });
                    }
                    return Ok(AcquiredPath::Existing(file));
                }
                AttemptResult::Unstable if attempt + 1 < MAX_ACQUISITION_ATTEMPTS => {}
                AttemptResult::Unstable => {
                    return Err(FsError::FileChanged {
                        path: path.value.clone(),
                        attempts: MAX_ACQUISITION_ATTEMPTS,
                    });
                }
            }
        }
        Err(FsError::InternalInvariant {
            invariant: "bounded_acquisition_loop_returns",
        })
    }

    fn acquire_existing_attempt<F>(
        &self,
        path: &WorkspaceRelativePath,
        validated: &ValidatedPath,
        limits: SnapshotLimits,
        accounting: &SnapshotAccounting,
        attempt: usize,
        after_read: &mut F,
    ) -> Result<AttemptResult, FsError>
    where
        F: FnMut(usize),
    {
        let mut file = match File::open(&validated.full_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AttemptResult::Unstable);
            }
            Err(error) => {
                return Err(io_error("open_snapshot_file", Some(&path.value), error));
            }
        };
        let metadata_before = file
            .metadata()
            .map_err(|error| io_error("inspect_open_snapshot", Some(&path.value), error))?;
        if !metadata_before.is_file() {
            return Ok(AttemptResult::Unstable);
        }
        let signature_before = MetadataSignature::from(&metadata_before);
        let ValidatedKind::Existing(validated_identity) = validated.kind else {
            return Ok(AttemptResult::Unstable);
        };
        if signature_before.identity != validated_identity {
            return Ok(AttemptResult::Unstable);
        }

        enforce_limit(
            "snapshot_file_bytes",
            metadata_before.len(),
            limits.file_bytes,
        )?;
        enforce_limit(
            "snapshot_bytes",
            checked_add(accounting.bytes, metadata_before.len(), "snapshot_bytes")?,
            limits.total_bytes,
        )?;
        let (bytes, digest) = read_and_hash_bounded(
            &mut file,
            limits.file_bytes,
            accounting.bytes,
            limits.total_bytes,
            path,
        )?;
        after_read(attempt);

        let metadata_after = file
            .metadata()
            .map_err(|error| io_error("reinspect_open_snapshot", Some(&path.value), error))?;
        let entry_after = match fs::symlink_metadata(&validated.full_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AttemptResult::Unstable);
            }
            Err(error) => {
                return Err(io_error("confirm_snapshot_entry", Some(&path.value), error));
            }
        };
        let parent_after = match fs::symlink_metadata(&validated.parent_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AttemptResult::Unstable);
            }
            Err(error) => {
                return Err(io_error(
                    "confirm_snapshot_parent",
                    Some(&path.value),
                    error,
                ));
            }
        };
        let stable = MetadataSignature::from(&metadata_after) == signature_before
            && entry_after.is_file()
            && metadata_identity(&entry_after) == signature_before.identity
            && parent_after.is_dir()
            && metadata_identity(&parent_after) == validated.parent_identity
            && metadata_after.len() == u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if !stable {
            return Ok(AttemptResult::Unstable);
        }

        let remaining_lines = limits.total_lines.saturating_sub(accounting.lines);
        let remaining_index_memory = limits
            .line_index_memory
            .saturating_sub(accounting.index_memory);
        let line_index =
            LineIndex::from_bytes_with_limits(&bytes, remaining_lines, remaining_index_memory)
                .map_err(|error| map_index_error(error, accounting))?;

        Ok(AttemptResult::Stable(FileSnapshot {
            id: SnapshotFileId(0),
            path: path.clone(),
            parent_identity: validated.parent_identity,
            parent_identities: Arc::clone(&validated.parent_identities),
            identity: signature_before.identity,
            link_count: metadata_before.nlink(),
            bytes: Arc::from(bytes),
            digest,
            line_index,
        }))
    }

    fn validate_path(&self, path: &WorkspaceRelativePath) -> Result<ValidatedPath, FsError> {
        let components = path.value.split('/').collect::<Vec<_>>();
        let basename = components
            .last()
            .ok_or_else(|| invalid_path(path, "path_empty"))?
            .to_string();
        let mut current = self.canonical_root.clone();
        let mut parent_identities = Vec::with_capacity(components.len());
        parent_identities.push(self.identity);

        for component in &components[..components.len() - 1] {
            current.push(component);
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(invalid_path(path, "missing_parent"));
                }
                Err(error) => {
                    return Err(io_error("inspect_path_parent", Some(&path.value), error));
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
            parent_identities.push(metadata_identity(&metadata));
        }

        let parent_path = current;
        let parent_metadata = fs::symlink_metadata(&parent_path)
            .map_err(|error| io_error("inspect_path_parent", Some(&path.value), error))?;
        if !parent_metadata.is_dir() {
            return Err(FsError::UnsupportedFileType {
                path: path.value.clone(),
            });
        }
        let parent_identity = metadata_identity(&parent_metadata);
        if parent_identities.last().copied() != Some(parent_identity) {
            parent_identities.push(parent_identity);
        }

        let full_path = parent_path.join(&basename);
        let kind = match fs::symlink_metadata(&full_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(FsError::SymlinkNotAllowed {
                    path: path.value.clone(),
                });
            }
            Ok(metadata) if metadata.is_file() => {
                ValidatedKind::Existing(metadata_identity(&metadata))
            }
            Ok(_) => {
                return Err(FsError::UnsupportedFileType {
                    path: path.value.clone(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => ValidatedKind::Absent,
            Err(error) => {
                return Err(io_error("inspect_path_entry", Some(&path.value), error));
            }
        };

        Ok(ValidatedPath {
            full_path,
            parent_path,
            parent_identity,
            parent_identities: parent_identities.into(),
            basename,
            kind,
        })
    }
}

/// Typed failures owned by the filesystem boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FsError {
    /// The running operating system cannot provide the required POSIX identity.
    UnsupportedPlatform,
    /// The canonical workspace root is not a directory.
    WorkspaceRootNotDirectory,
    /// A workspace-relative path violates normalization or parent rules.
    InvalidPath {
        /// Safe workspace-relative spelling.
        path: String,
        /// Stable validation reason.
        reason: &'static str,
    },
    /// A path beneath the canonical root traverses or names a symbolic link.
    SymlinkNotAllowed {
        /// Safe workspace-relative spelling.
        path: String,
    },
    /// A path component or final object has an unsupported file type.
    UnsupportedFileType {
        /// Safe workspace-relative spelling.
        path: String,
    },
    /// A required existing workspace-relative path is absent.
    PathNotFound {
        /// Normalized workspace-relative spelling.
        path: String,
    },
    /// A stable observed state does not match its explicit precondition.
    PreconditionFailed {
        /// Safe workspace-relative spelling.
        path: String,
        /// Expected digest, or `None` when absence was required.
        expected: Option<Sha256Digest>,
        /// Observed digest when a stable regular file was read.
        actual: Option<Sha256Digest>,
    },
    /// One normalized path was supplied with incompatible preconditions.
    IncompatiblePrecondition {
        /// Safe workspace-relative spelling.
        path: String,
    },
    /// Distinct normalized paths identify one physical file.
    FileAlias {
        /// First normalized spelling.
        first_path: String,
        /// Conflicting normalized spelling.
        second_path: String,
    },
    /// The file remained demonstrably unstable through the bounded retry policy.
    FileChanged {
        /// Safe workspace-relative spelling.
        path: String,
        /// Total acquisition attempts made.
        attempts: usize,
    },
    /// A configured snapshot or index resource limit was exceeded.
    ResourceLimitExceeded {
        /// Stable resource name.
        resource: &'static str,
        /// Observed or projected usage.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// The workspace control tree and a changed target parent are on different devices.
    CrossDeviceTransaction,
    /// The platform did not provide the required no-replace rename primitive.
    NoReplaceUnavailable,
    /// The selected workspace is not on a qualified v0.1 local filesystem.
    UnsupportedFilesystem {
        /// Stable observed filesystem name or numeric type.
        filesystem: String,
    },
    /// Another process holds an incompatible workspace control lock.
    TransactionBusy,
    /// One or more active transaction directories require explicit recovery.
    TransactionRecoveryRequired {
        /// Canonical active transaction identifiers, sorted lexicographically.
        transaction_ids: Vec<String>,
    },
    /// A requested canonical transaction identifier does not exist.
    TransactionNotFound {
        /// Requested identifier.
        transaction_id: String,
    },
    /// The requested recovery action is not safe for the current journal state.
    RecoveryActionNotAllowed {
        /// Canonical transaction identifier.
        transaction_id: String,
        /// Stable reason for refusing the action.
        reason: &'static str,
    },
    /// The control tree violates its ownership, type, naming, or permission rules.
    ControlDirectoryInvalid {
        /// Stable validation reason.
        reason: &'static str,
    },
    /// A legacy control directory from an earlier incompatible release exists
    /// at the workspace root.
    LegacyControlState,
    /// A persistent transaction record or directory is corrupt.
    TransactionRecordCorrupt {
        /// Canonical transaction identifier when one could be established.
        transaction_id: Option<String>,
        /// Stable corruption reason.
        reason: &'static str,
    },
    /// Filesystem observations cannot be reconciled with the journal.
    RecoveryConflict {
        /// Stable classification reason.
        reason: &'static str,
    },
    /// A filesystem read or metadata operation failed.
    Io {
        /// Stable operation name.
        operation: &'static str,
        /// Safe workspace-relative path when applicable.
        path: Option<String>,
        /// Standard I/O error classification.
        kind: io::ErrorKind,
    },
    /// A filesystem-independent core failure propagated through this boundary.
    Core(CoreError),
    /// An internal invariant failed without a corresponding external cause.
    InternalInvariant {
        /// Stable invariant name.
        invariant: &'static str,
    },
}

impl fmt::Display for FsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(formatter, "unsupported platform"),
            Self::WorkspaceRootNotDirectory => {
                write!(formatter, "workspace root is not a directory")
            }
            Self::InvalidPath { path, reason } => {
                write!(formatter, "invalid path {path:?}: {reason}")
            }
            Self::SymlinkNotAllowed { path } => {
                write!(formatter, "symbolic link is not allowed: {path}")
            }
            Self::UnsupportedFileType { path } => {
                write!(formatter, "unsupported file type: {path}")
            }
            Self::PathNotFound { path } => write!(formatter, "path not found: {path}"),
            Self::PreconditionFailed { path, .. } => {
                write!(formatter, "precondition failed: {path}")
            }
            Self::IncompatiblePrecondition { path } => {
                write!(formatter, "incompatible preconditions: {path}")
            }
            Self::FileAlias {
                first_path,
                second_path,
            } => write!(formatter, "file alias: {first_path} and {second_path}"),
            Self::FileChanged { path, attempts } => write!(
                formatter,
                "file changed during {attempts} acquisition attempts: {path}"
            ),
            Self::ResourceLimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "resource limit exceeded for {resource}: {actual} > {limit}"
            ),
            Self::CrossDeviceTransaction => {
                write!(formatter, "transaction paths are on different filesystems")
            }
            Self::NoReplaceUnavailable => {
                write!(formatter, "no-replace rename is unavailable")
            }
            Self::UnsupportedFilesystem { filesystem } => {
                write!(formatter, "unsupported filesystem: {filesystem}")
            }
            Self::TransactionBusy => write!(formatter, "workspace transaction lock is busy"),
            Self::TransactionRecoveryRequired { transaction_ids } => write!(
                formatter,
                "unfinished transactions require recovery: {}",
                transaction_ids.join(", ")
            ),
            Self::TransactionNotFound { transaction_id } => {
                write!(formatter, "transaction not found: {transaction_id}")
            }
            Self::RecoveryActionNotAllowed {
                transaction_id,
                reason,
            } => write!(
                formatter,
                "recovery action is not allowed for {transaction_id}: {reason}"
            ),
            Self::ControlDirectoryInvalid { reason } => {
                write!(formatter, "invalid control directory: {reason}")
            }
            Self::LegacyControlState => write!(
                formatter,
                "legacy control state from an earlier incompatible release is present"
            ),
            Self::TransactionRecordCorrupt {
                transaction_id,
                reason,
            } => write!(
                formatter,
                "transaction record is corrupt{}: {reason}",
                transaction_id
                    .as_deref()
                    .map(|id| format!(" ({id})"))
                    .unwrap_or_default()
            ),
            Self::RecoveryConflict { reason } => {
                write!(formatter, "recovery classification conflict: {reason}")
            }
            Self::Io {
                operation, kind, ..
            } => write!(formatter, "I/O failure during {operation}: {kind:?}"),
            Self::Core(error) => write!(formatter, "core error: {error}"),
            Self::InternalInvariant { invariant } => {
                write!(formatter, "internal invariant failed: {invariant}")
            }
        }
    }
}

impl Error for FsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Core(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CoreError> for FsError {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

/// Captures the process umask for deriving the v0.1 new-file permission mode.
///
/// Call this once at CLI startup, before any worker threads are created.
#[must_use]
pub fn capture_startup_umask() -> u32 {
    use rustix::fs::Mode;
    use rustix::process::umask;

    let previous = umask(Mode::empty());
    umask(previous);
    #[cfg(target_os = "linux")]
    {
        previous.bits()
    }
    #[cfg(target_os = "macos")]
    {
        u32::from(previous.bits())
    }
}

#[cfg(debug_assertions)]
pub(crate) fn test_failpoint(name: &str) -> Result<(), FsError> {
    if std::env::var_os("SRCMV_TEST_FAILPOINT").is_some_and(|value| {
        value
            .to_string_lossy()
            .split(',')
            .any(|configured| configured == name)
    }) {
        if std::env::var_os("SRCMV_TEST_FAILPOINT_ACTION").is_some_and(|value| value == "exit") {
            std::process::exit(86);
        }
        return Err(FsError::Io {
            operation: "test_failpoint",
            path: None,
            kind: io::ErrorKind::Interrupted,
        });
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
pub(crate) const fn test_failpoint(_name: &str) -> Result<(), FsError> {
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidatedPath {
    full_path: PathBuf,
    parent_path: PathBuf,
    parent_identity: FileIdentity,
    parent_identities: Arc<[FileIdentity]>,
    basename: String,
    kind: ValidatedKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValidatedKind {
    Existing(FileIdentity),
    Absent,
}

enum AcquiredPath {
    Existing(FileSnapshot),
    Absent(AbsentPathSnapshot),
}

enum AttemptResult {
    Stable(FileSnapshot),
    Unstable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MetadataSignature {
    identity: FileIdentity,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    links: u64,
}

impl From<&Metadata> for MetadataSignature {
    fn from(metadata: &Metadata) -> Self {
        Self {
            identity: metadata_identity(metadata),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            links: metadata.nlink(),
        }
    }
}

#[derive(Default)]
struct SnapshotAccounting {
    identities: u64,
    bytes: u64,
    lines: u64,
    index_memory: u64,
}

impl SnapshotAccounting {
    fn charge_identity(&mut self, limits: SnapshotLimits) -> Result<(), FsError> {
        self.identities = checked_add(self.identities, 1, "snapshot_identities")?;
        enforce_limit("snapshot_identities", self.identities, limits.identities)
    }

    fn charge_file(&mut self, file: &FileSnapshot, limits: SnapshotLimits) -> Result<(), FsError> {
        self.bytes = checked_add(
            self.bytes,
            u64::try_from(file.bytes.len()).unwrap_or(u64::MAX),
            "snapshot_bytes",
        )?;
        enforce_limit("snapshot_bytes", self.bytes, limits.total_bytes)?;
        self.lines = checked_add(self.lines, file.line_index.line_count(), "line_count")?;
        enforce_limit("line_count", self.lines, limits.total_lines)?;
        self.index_memory = checked_add(
            self.index_memory,
            file.line_index.memory_bytes(),
            "line_index_memory",
        )?;
        enforce_limit(
            "line_index_memory",
            self.index_memory,
            limits.line_index_memory,
        )
    }
}

fn parse_relative_path(value: &str, limit: u64) -> Result<WorkspaceRelativePath, FsError> {
    enforce_limit(
        "path_bytes",
        u64::try_from(value.len()).unwrap_or(u64::MAX),
        limit,
    )?;
    if value.is_empty() {
        return Err(FsError::InvalidPath {
            path: value.to_string(),
            reason: "path_empty",
        });
    }
    if value.contains('\0') {
        return Err(FsError::InvalidPath {
            path: value.to_string(),
            reason: "path_contains_nul",
        });
    }
    if Path::new(value).is_absolute() || value.starts_with('/') {
        return Err(FsError::InvalidPath {
            path: value.to_string(),
            reason: "path_absolute",
        });
    }
    let components = value.split('/').collect::<Vec<_>>();
    if components.iter().any(|component| component.is_empty()) {
        return Err(FsError::InvalidPath {
            path: value.to_string(),
            reason: "path_empty_component",
        });
    }
    if components.contains(&".") {
        return Err(FsError::InvalidPath {
            path: value.to_string(),
            reason: "path_current_component",
        });
    }
    if components.contains(&"..") {
        return Err(FsError::InvalidPath {
            path: value.to_string(),
            reason: "path_parent_component",
        });
    }
    if components[0].eq_ignore_ascii_case(".srcmv") {
        return Err(FsError::InvalidPath {
            path: value.to_string(),
            reason: "reserved_path",
        });
    }
    Ok(WorkspaceRelativePath {
        value: value.to_string(),
    })
}

fn metadata_identity(metadata: &Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn hash_identity(identity: FileIdentity) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(b"srcmv-physical-identity-v1\0");
    hasher.update(identity.device.to_be_bytes());
    hasher.update(identity.inode.to_be_bytes());
    Sha256Digest(hasher.finalize().into())
}

fn read_and_hash_bounded(
    file: &mut File,
    file_limit: u64,
    aggregate_used: u64,
    aggregate_limit: u64,
    path: &WorkspaceRelativePath,
) -> Result<(Vec<u8>, Sha256Digest), FsError> {
    let aggregate_remaining = aggregate_limit.saturating_sub(aggregate_used);
    let read_limit = file_limit.min(aggregate_remaining);
    let mut reader = file.take(read_limit.saturating_add(1));
    let mut bytes = Vec::new();
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| io_error("read_snapshot_file", Some(&path.value), error))?;
        if count == 0 {
            break;
        }
        let projected = checked_add(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            u64::try_from(count).unwrap_or(u64::MAX),
            "snapshot_file_bytes",
        )?;
        enforce_limit("snapshot_file_bytes", projected, file_limit)?;
        enforce_limit(
            "snapshot_bytes",
            checked_add(aggregate_used, projected, "snapshot_bytes")?,
            aggregate_limit,
        )?;
        bytes.extend_from_slice(&buffer[..count]);
        hasher.update(&buffer[..count]);
    }
    Ok((bytes, Sha256Digest(hasher.finalize().into())))
}

fn reject_alias(
    aliases: &mut HashMap<FileIdentity, String>,
    identity: FileIdentity,
    path: &WorkspaceRelativePath,
) -> Result<(), FsError> {
    if let Some(first_path) = aliases.get(&identity) {
        if first_path != &path.value {
            return Err(FsError::FileAlias {
                first_path: first_path.clone(),
                second_path: path.value.clone(),
            });
        }
    } else {
        aliases.insert(identity, path.value.clone());
    }
    Ok(())
}

fn map_index_error(error: CoreError, accounting: &SnapshotAccounting) -> FsError {
    match error {
        CoreError::ResourceLimitExceeded {
            resource,
            actual,
            limit,
        } => {
            let prior = match resource {
                "line_count" => accounting.lines,
                "line_index_memory" => accounting.index_memory,
                _ => 0,
            };
            FsError::ResourceLimitExceeded {
                resource,
                actual: prior.saturating_add(actual),
                limit: prior.saturating_add(limit),
            }
        }
        other => FsError::Core(other),
    }
}

fn checked_add(left: u64, right: u64, resource: &'static str) -> Result<u64, FsError> {
    left.checked_add(right)
        .ok_or(FsError::ResourceLimitExceeded {
            resource,
            actual: u64::MAX,
            limit: u64::MAX - 1,
        })
}

fn enforce_limit(resource: &'static str, actual: u64, limit: u64) -> Result<(), FsError> {
    if actual > limit {
        return Err(FsError::ResourceLimitExceeded {
            resource,
            actual,
            limit,
        });
    }
    Ok(())
}

fn invalid_path(path: &WorkspaceRelativePath, reason: &'static str) -> FsError {
    FsError::InvalidPath {
        path: path.value.clone(),
        reason,
    }
}

fn io_error(operation: &'static str, path: Option<&str>, error: io::Error) -> FsError {
    FsError::Io {
        operation,
        path: path.map(str::to_string),
        kind: error.kind(),
    }
}

fn ensure_supported_platform() -> Result<(), FsError> {
    if cfg!(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64")
    )) {
        Ok(())
    } else {
        Err(FsError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod snapshot_tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestWorkspace(PathBuf);

    impl TestWorkspace {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("srcmv-phase3-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).expect("test workspace should be created");
            Self(path)
        }

        fn open(&self) -> Workspace {
            Workspace::open(&self.0).expect("test workspace should open")
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("test workspace should be removed");
        }
    }

    fn path(value: &str) -> WorkspaceRelativePath {
        WorkspaceRelativePath {
            value: value.to_string(),
        }
    }

    fn digest(bytes: &[u8]) -> Sha256Digest {
        Sha256Digest(Sha256::digest(bytes).into())
    }

    fn existing(value: &str, bytes: &[u8]) -> SnapshotRequirement {
        SnapshotRequirement {
            path: path(value),
            state: RequiredPathState::Existing(digest(bytes)),
        }
    }

    #[test]
    fn snapshot_should_capture_existing_bytes_identity_links_and_absence() {
        let fixture = TestWorkspace::new();
        fs::create_dir(fixture.0.join("src")).expect("parent should be created");
        fs::write(fixture.0.join("src/input"), b"a\r\nb\rc\n").expect("fixture should be written");
        let workspace = fixture.open();
        let snapshot = workspace
            .acquire_snapshot(
                &[
                    existing("src/input", b"a\r\nb\rc\n"),
                    SnapshotRequirement {
                        path: path("src/new"),
                        state: RequiredPathState::Absent,
                    },
                ],
                SnapshotLimits::default(),
            )
            .expect("snapshot should be acquired");

        assert_eq!(snapshot.workspace_identity, workspace.identity());
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(&*snapshot.files[0].bytes, b"a\r\nb\rc\n");
        assert_eq!(snapshot.files[0].digest, digest(b"a\r\nb\rc\n"));
        assert_eq!(snapshot.files[0].line_index.line_count(), 3);
        assert_eq!(snapshot.files[0].link_count, 1);
        assert_eq!(snapshot.files[0].parent_identities.len(), 2);
        assert_eq!(snapshot.absent_paths.len(), 1);
        assert_eq!(snapshot.absent_paths[0].basename, "new");
        assert_eq!(snapshot.absent_paths[0].parent_identities.len(), 2);
    }

    #[test]
    fn snapshot_should_retain_multiply_linked_copy_source_and_reject_aliases() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("first"), b"bytes").expect("fixture should be written");
        fs::hard_link(fixture.0.join("first"), fixture.0.join("second"))
            .expect("hard link should be created");
        let workspace = fixture.open();
        let one = workspace
            .acquire_snapshot(&[existing("first", b"bytes")], SnapshotLimits::default())
            .expect("multiply linked input remains readable");
        let alias_error = workspace
            .acquire_snapshot(
                &[existing("first", b"bytes"), existing("second", b"bytes")],
                SnapshotLimits::default(),
            )
            .expect_err("distinct aliases should fail");

        assert_eq!(one.files[0].link_count, 2);
        assert!(matches!(alias_error, FsError::FileAlias { .. }));
    }

    #[test]
    fn snapshot_should_reject_invalid_reserved_symlink_special_and_missing_parent_paths() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("real"), b"x").expect("fixture should be written");
        symlink("real", fixture.0.join("link")).expect("symlink should be created");
        fs::create_dir(fixture.0.join("real_parent")).expect("parent should be created");
        symlink("real_parent", fixture.0.join("parent_link"))
            .expect("parent symlink should be created");
        fs::create_dir(fixture.0.join("special")).expect("special directory should be created");
        let workspace = fixture.open();

        for (value, matcher) in [
            ("", "invalid"),
            ("a//b", "invalid"),
            ("a/./b", "invalid"),
            ("../real", "invalid"),
            (".SRCMV/lock", "invalid"),
            ("missing/child", "invalid"),
            ("link", "symlink"),
            ("parent_link/child", "symlink"),
            ("special", "special"),
        ] {
            let error = workspace
                .inspect(&[value.to_string()], SnapshotLimits::default())
                .expect_err("path should be rejected");
            match matcher {
                "invalid" => assert!(matches!(error, FsError::InvalidPath { .. }), "{value}"),
                "symlink" => assert!(
                    matches!(error, FsError::SymlinkNotAllowed { .. }),
                    "{value}"
                ),
                "special" => assert!(
                    matches!(error, FsError::UnsupportedFileType { .. }),
                    "{value}"
                ),
                _ => unreachable!("test matcher is exhaustive"),
            }
        }
    }

    #[test]
    fn snapshot_should_reject_a_stable_wrong_digest_without_retrying() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("file"), b"actual").expect("fixture should be written");
        let workspace = fixture.open();
        let mut hooks = 0;
        let error = workspace
            .acquire_snapshot_with_hook(
                &[existing("file", b"expected")],
                SnapshotLimits::default(),
                |_, _| hooks += 1,
            )
            .expect_err("wrong digest should fail");

        assert!(matches!(error, FsError::PreconditionFailed { .. }));
        assert_eq!(hooks, 1);
    }

    #[test]
    fn snapshot_should_retry_one_unstable_read_then_succeed() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("file"), b"stable").expect("fixture should be written");
        let workspace = fixture.open();
        let mut hooks = 0;
        let snapshot = workspace
            .acquire_snapshot_with_hook(
                &[existing("file", b"stable!")],
                SnapshotLimits::default(),
                |attempt, path| {
                    hooks += 1;
                    if attempt == 0 {
                        OpenOptions::new()
                            .append(true)
                            .open(path)
                            .expect("fixture should reopen")
                            .write_all(b"!")
                            .expect("fixture should mutate");
                    }
                },
            )
            .expect("second stable attempt should succeed");

        assert_eq!(hooks, 2);
        assert_eq!(&*snapshot.files[0].bytes, b"stable!");
    }

    #[test]
    fn snapshot_should_retry_when_parent_entry_identity_changes_after_read() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("file"), b"old").expect("fixture should be written");
        let workspace = fixture.open();
        let mut hooks = 0;
        let snapshot = workspace
            .acquire_snapshot_with_hook(
                &[existing("file", b"new")],
                SnapshotLimits::default(),
                |attempt, path| {
                    hooks += 1;
                    if attempt == 0 {
                        fs::remove_file(path).expect("old entry should be removed");
                        fs::write(path, b"new").expect("new entry should be installed");
                    }
                },
            )
            .expect("replacement should be acquired on the second attempt");

        assert_eq!(hooks, 2);
        assert_eq!(&*snapshot.files[0].bytes, b"new");
    }

    #[test]
    fn snapshot_should_stop_after_three_demonstrably_unstable_reads() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("file"), b"x").expect("fixture should be written");
        let workspace = fixture.open();
        let mut hooks = 0;
        let error = workspace
            .acquire_snapshot_with_hook(
                &[existing("file", b"never-matches")],
                SnapshotLimits::default(),
                |_, path| {
                    hooks += 1;
                    OpenOptions::new()
                        .append(true)
                        .open(path)
                        .expect("fixture should reopen")
                        .write_all(b"x")
                        .expect("fixture should mutate");
                },
            )
            .expect_err("three unstable attempts should fail");

        assert!(matches!(error, FsError::FileChanged { attempts: 3, .. }));
        assert_eq!(hooks, 3);
    }

    #[test]
    fn snapshot_should_enforce_file_aggregate_line_index_and_identity_limits() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("one"), b"a\n").expect("fixture should be written");
        fs::write(fixture.0.join("two"), b"b\n").expect("fixture should be written");
        let workspace = fixture.open();
        let requirements = [existing("one", b"a\n"), existing("two", b"b\n")];

        for limits in [
            SnapshotLimits::new(4_096, 1, 100, 100, 100, 100),
            SnapshotLimits::new(4_096, 10, 1, 100, 100, 100),
            SnapshotLimits::new(4_096, 10, 100, 3, 100, 100),
            SnapshotLimits::new(4_096, 10, 100, 100, 1, 100),
            SnapshotLimits::new(4_096, 10, 100, 100, 100, 15),
        ] {
            assert!(matches!(
                workspace.acquire_snapshot(&requirements, limits),
                Err(FsError::ResourceLimitExceeded { .. })
            ));
        }
    }

    #[test]
    fn phase9_snapshot_limits_cover_below_at_and_above_every_boundary() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("one"), b"a\n").expect("fixture should be written");
        fs::write(fixture.0.join("two"), b"b\n").expect("fixture should be written");
        let workspace = fixture.open();
        let requirements = [existing("one", b"a\n"), existing("two", b"b\n")];
        let exact = SnapshotLimits::new(3, 2, 2, 4, 2, 16);

        workspace
            .acquire_snapshot(&requirements, SnapshotLimits::default())
            .expect("usage below release limits should pass");
        workspace
            .acquire_snapshot(&requirements, exact)
            .expect("usage at every limit should pass");

        for limits in [
            SnapshotLimits::new(2, 2, 2, 4, 2, 16),
            SnapshotLimits::new(3, 1, 2, 4, 2, 16),
            SnapshotLimits::new(3, 2, 1, 4, 2, 16),
            SnapshotLimits::new(3, 2, 2, 3, 2, 16),
            SnapshotLimits::new(3, 2, 2, 4, 1, 16),
            SnapshotLimits::new(3, 2, 2, 4, 2, 15),
        ] {
            assert!(matches!(
                workspace.acquire_snapshot(&requirements, limits),
                Err(FsError::ResourceLimitExceeded { .. })
            ));
        }
    }

    #[test]
    fn inspect_should_report_regular_absent_and_non_utf8_content_without_writing() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("binary"), [0xff, b'\r', b'\n'])
            .expect("fixture should be written");
        let workspace = fixture.open();
        let before = directory_entries(&fixture.0);
        let inspections = workspace
            .inspect(
                &["binary".to_string(), "absent".to_string()],
                SnapshotLimits::default(),
            )
            .expect("inspection should succeed");
        let after = directory_entries(&fixture.0);

        assert!(matches!(
            inspections[0].state,
            InspectedState::Existing {
                byte_length: 3,
                line_count: 1,
                ..
            }
        ));
        assert_eq!(inspections[1].state, InspectedState::Absent);
        assert_eq!(before, after);
    }

    #[test]
    fn snapshot_acquisition_should_not_modify_or_create_workspace_entries() {
        let fixture = TestWorkspace::new();
        fs::write(fixture.0.join("file"), b"immutable\n").expect("fixture should be written");
        let workspace = fixture.open();
        let before_entries = directory_entries(&fixture.0);
        let before_metadata =
            fs::metadata(fixture.0.join("file")).expect("fixture metadata should be readable");

        workspace
            .acquire_snapshot(
                &[
                    existing("file", b"immutable\n"),
                    SnapshotRequirement {
                        path: path("absent"),
                        state: RequiredPathState::Absent,
                    },
                ],
                SnapshotLimits::default(),
            )
            .expect("snapshot should be acquired");

        let after_entries = directory_entries(&fixture.0);
        let after_metadata =
            fs::metadata(fixture.0.join("file")).expect("fixture metadata should be readable");
        assert_eq!(before_entries, after_entries);
        assert_eq!(before_metadata.len(), after_metadata.len());
        assert_eq!(before_metadata.mode(), after_metadata.mode());
        assert_eq!(before_metadata.mtime(), after_metadata.mtime());
        assert_eq!(before_metadata.mtime_nsec(), after_metadata.mtime_nsec());
        assert!(!fixture.0.join(".srcmv").exists());
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
}
