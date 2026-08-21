//! Explicit, external Ed25519 signing-key and trusted-head lifecycle helpers.
//!
//! These helpers intentionally never choose a path on the caller's behalf. Signing
//! keys and trusted heads must live outside the library tree, in an existing private
//! directory owned by the current user. The trusted head authenticates captured
//! bytes since capture; it is not proof that upstream content is true or complete.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use wikisync_integrity::{
    MAX_TRUSTED_HEAD_BYTES, ManifestSigningKey, TrustedManifestHead, VerificationFindingKind,
    VerificationOptions, VerificationReport, VerificationScope, sign_current_manifest_head,
    verify_library_against_trusted_head,
};
use wikisync_store::{Library, ManifestId};

/// Maximum accepted PKCS#8 signing-key document size.
pub const MAX_SIGNING_KEY_BYTES: usize = 16 * 1024;

/// Whether a trusted-head export must be new or may refresh an existing anchor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnchorWriteMode {
    /// Create a new file and fail rather than overwrite any existing entry.
    CreateNew,
    /// Atomically replace an existing, canonical trusted-head file.
    RefreshExisting,
}

/// Non-secret result of persisting or validating a signing key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyFileSummary {
    /// Size of the validated PKCS#8 document.
    pub byte_length: usize,
}

/// Non-secret identity of an exported trusted manifest head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedHeadSummary {
    /// Manifest append sequence authenticated by the anchor.
    pub sequence: u64,
    /// Identity of the exact canonical manifest body.
    pub manifest_id: ManifestId,
    /// Public Ed25519 verification key embedded in the anchor.
    pub public_key: [u8; 32],
}

impl From<&TrustedManifestHead> for TrustedHeadSummary {
    fn from(anchor: &TrustedManifestHead) -> Self {
        Self {
            sequence: anchor.sequence,
            manifest_id: anchor.manifest_id,
            public_key: *anchor.public_key(),
        }
    }
}

/// Result of comparing an external anchor with a full local verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnchorComparison {
    /// The signature, exact current manifest head, and full local verification agree.
    AuthenticatedCurrent,
    /// The anchor's Ed25519 signature is invalid.
    InvalidSignature,
    /// The signature is valid, but the anchor is stale or names another manifest head.
    DifferentHead,
    /// The anchor comparison ran, but other full-verification findings remain.
    LocalVerificationFailed,
}

/// A parsed anchor and the full verification report used to compare it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnchorInspection {
    /// Public anchor identity safe to show in CLI output.
    pub anchor: TrustedHeadSummary,
    /// High-level comparison result.
    pub comparison: AnchorComparison,
    /// Complete bounded full-verification report for structured CLI/JSON output.
    pub report: VerificationReport,
}

/// Durable phase reached by a failed key rotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RotationPhase {
    /// No rotation output is known to have been created.
    Preflight,
    /// The new key is durable, while the current anchor is still the old anchor.
    NewKeyDurable,
    /// The new key and recovery copy are durable; the current anchor is still old.
    RecoveryAnchorDurable,
    /// Replacement was renamed into place, but final directory durability was uncertain.
    CurrentAnchorReplaced,
}

/// Result of a completed signing-key rotation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RotationSummary {
    /// Trusted head retained at the recovery path.
    pub previous: TrustedHeadSummary,
    /// Trusted head installed at the current anchor path.
    pub current: TrustedHeadSummary,
}

/// A redacted failure from the external trust lifecycle.
#[derive(Debug)]
pub enum TrustError {
    /// A path was relative, inside the library, a symlink, or otherwise unsafe.
    UnsafePath(&'static str),
    /// A private key or its parent directory was not private and user-owned.
    InsecurePermissions(&'static str),
    /// A create-new target already exists.
    AlreadyExists(&'static str),
    /// A bounded input exceeded its fixed maximum size.
    InputTooLarge(&'static str),
    /// PKCS#8 validation rejected the input without exposing its contents.
    InvalidSigningKey,
    /// Trusted-head parsing rejected the input without echoing it.
    InvalidTrustedHead,
    /// Signing the current validated manifest chain failed.
    SigningFailed,
    /// Full verification or anchor comparison could not be completed.
    VerificationFailed,
    /// Full verification completed but did not authenticate the signed exact head.
    LibraryNotFullyVerified {
        /// Total findings retained or counted by the bounded verification report.
        finding_count: u64,
    },
    /// Rotation requires an anchor that authenticates the current fully verified library.
    CurrentAnchorNotAuthenticated,
    /// A local I/O operation failed; paths and input bytes are deliberately omitted.
    Io {
        /// Fixed operation label with no user-supplied data.
        operation: &'static str,
        /// Coarse operating-system error category.
        kind: io::ErrorKind,
    },
}

impl fmt::Display for TrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath(reason) => write!(formatter, "unsafe external trust path: {reason}"),
            Self::InsecurePermissions(reason) => {
                write!(formatter, "external trust storage is not private: {reason}")
            }
            Self::AlreadyExists(kind) => write!(formatter, "{kind} target already exists"),
            Self::InputTooLarge(kind) => write!(formatter, "{kind} exceeds its byte limit"),
            Self::InvalidSigningKey => formatter.write_str("invalid Ed25519 PKCS#8 signing key"),
            Self::InvalidTrustedHead => formatter.write_str("invalid canonical trusted head"),
            Self::SigningFailed => {
                formatter.write_str("could not sign the current validated manifest head")
            }
            Self::VerificationFailed => {
                formatter.write_str("could not complete full trusted-head verification")
            }
            Self::LibraryNotFullyVerified { finding_count } => write!(
                formatter,
                "refusing to publish a trusted head because full verification reported {finding_count} finding(s) or incomplete coverage"
            ),
            Self::CurrentAnchorNotAuthenticated => formatter.write_str(
                "current anchor does not authenticate the current fully verified library",
            ),
            Self::Io { operation, kind } => {
                write!(
                    formatter,
                    "external trust storage failed while {operation}: {kind}"
                )
            }
        }
    }
}

impl Error for TrustError {}

/// A failed rotation plus the durable phase reached before the failure.
#[derive(Debug)]
pub struct RotationError {
    /// Last durable phase known to have completed.
    pub phase: RotationPhase,
    /// Redacted underlying lifecycle failure.
    pub error: TrustError,
}

impl fmt::Display for RotationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "key rotation stopped after {:?}: {}",
            self.phase, self.error
        )
    }
}

impl Error for RotationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

/// Generates a new Ed25519 PKCS#8 key at an explicit external create-new path.
pub fn generate_signing_key(
    library_root: &Path,
    destination: &Path,
) -> Result<KeyFileSummary, TrustError> {
    validate_external_target(library_root, destination, StorageSensitivity::Secret)?;
    ensure_absent(destination, "signing key")?;
    let key = ManifestSigningKey::generate().map_err(|_| TrustError::SigningFailed)?;
    let bytes = key.to_pkcs8_bytes();
    write_create_new(destination, &bytes, "creating signing key")?;
    Ok(KeyFileSummary {
        byte_length: bytes.len(),
    })
}

/// Validates and loads one external, user-private Ed25519 PKCS#8 key.
pub fn validate_signing_key(
    library_root: &Path,
    path: &Path,
) -> Result<ManifestSigningKey, TrustError> {
    validate_external_target(library_root, path, StorageSensitivity::Secret)?;
    let bytes = read_bounded_file(path, MAX_SIGNING_KEY_BYTES, FileKind::SigningKey)?;
    ManifestSigningKey::from_pkcs8(&bytes).map_err(|_| TrustError::InvalidSigningKey)
}

/// Validates an external key and copies it to another external create-new path.
///
/// The source is deliberately retained, so importing cannot silently turn the new
/// destination into the only copy of existing key material.
pub fn import_signing_key(
    library_root: &Path,
    source: &Path,
    destination: &Path,
) -> Result<KeyFileSummary, TrustError> {
    if paths_alias(source, destination)? {
        return Err(TrustError::UnsafePath(
            "key import source and destination must differ",
        ));
    }
    validate_external_target(library_root, source, StorageSensitivity::Secret)?;
    validate_external_target(library_root, destination, StorageSensitivity::Secret)?;
    ensure_absent(destination, "signing key")?;
    let bytes = read_bounded_file(source, MAX_SIGNING_KEY_BYTES, FileKind::SigningKey)?;
    ManifestSigningKey::from_pkcs8(&bytes).map_err(|_| TrustError::InvalidSigningKey)?;
    write_create_new(destination, &bytes, "importing signing key")?;
    Ok(KeyFileSummary {
        byte_length: bytes.len(),
    })
}

/// Signs and durably exports the current validated manifest head.
pub fn export_current_trusted_head(
    library: &Library,
    signing_key_path: &Path,
    anchor_path: &Path,
    mode: AnchorWriteMode,
) -> Result<TrustedHeadSummary, TrustError> {
    let key = validate_signing_key(library.root(), signing_key_path)?;
    validate_external_target(library.root(), anchor_path, StorageSensitivity::Anchor)?;
    if paths_alias(signing_key_path, anchor_path)? {
        return Err(TrustError::UnsafePath(
            "signing key and trusted head must use different paths",
        ));
    }
    let anchor = sign_fully_verified_head(library, &key)?;
    let bytes = anchor
        .to_canonical_json()
        .map_err(|_| TrustError::InvalidTrustedHead)?;
    write_anchor(anchor_path, &bytes, mode)?;
    Ok(TrustedHeadSummary::from(&anchor))
}

/// Reads an external anchor and compares it against bounded full verification.
pub fn inspect_trusted_head(
    library: &Library,
    anchor_path: &Path,
) -> Result<AnchorInspection, TrustError> {
    validate_external_target(library.root(), anchor_path, StorageSensitivity::Anchor)?;
    let bytes = read_bounded_file(anchor_path, MAX_TRUSTED_HEAD_BYTES, FileKind::TrustedHead)?;
    let anchor = TrustedManifestHead::from_canonical_json(&bytes)
        .map_err(|_| TrustError::InvalidTrustedHead)?;
    let report = verify_library_against_trusted_head(
        library,
        VerificationOptions::new(VerificationScope::Full),
        &anchor,
    )
    .map_err(|_| TrustError::VerificationFailed)?;
    let comparison = classify_report(&report);
    Ok(AnchorInspection {
        anchor: TrustedHeadSummary::from(&anchor),
        comparison,
        report,
    })
}

/// Rotates to a create-new key while retaining the old anchor at a recovery path.
///
/// Rotation first requires the current anchor to authenticate a full verification.
/// It then durably creates the new key, durably copies the old canonical anchor to a
/// create-new recovery path, and finally atomically replaces the current anchor.
/// Neither key is deleted. [`RotationError::phase`] tells a caller which recovery
/// artifacts are durable if an operation stops partway through.
pub fn rotate_signing_key(
    library: &Library,
    current_anchor_path: &Path,
    new_key_path: &Path,
    recovery_anchor_path: &Path,
) -> Result<RotationSummary, RotationError> {
    let result = rotate_signing_key_inner(
        library,
        current_anchor_path,
        new_key_path,
        recovery_anchor_path,
    );
    result.map_err(|(phase, error)| RotationError { phase, error })
}

fn rotate_signing_key_inner(
    library: &Library,
    current_anchor_path: &Path,
    new_key_path: &Path,
    recovery_anchor_path: &Path,
) -> Result<RotationSummary, (RotationPhase, TrustError)> {
    let preflight = || RotationPhase::Preflight;
    if paths_alias(current_anchor_path, new_key_path).map_err(|error| (preflight(), error))?
        || paths_alias(current_anchor_path, recovery_anchor_path)
            .map_err(|error| (preflight(), error))?
        || paths_alias(new_key_path, recovery_anchor_path).map_err(|error| (preflight(), error))?
    {
        return Err((
            preflight(),
            TrustError::UnsafePath("rotation paths must all differ"),
        ));
    }
    let inspection =
        inspect_trusted_head(library, current_anchor_path).map_err(|error| (preflight(), error))?;
    if inspection.comparison != AnchorComparison::AuthenticatedCurrent {
        return Err((preflight(), TrustError::CurrentAnchorNotAuthenticated));
    }
    validate_external_target(library.root(), new_key_path, StorageSensitivity::Secret)
        .map_err(|error| (preflight(), error))?;
    validate_external_target(
        library.root(),
        recovery_anchor_path,
        StorageSensitivity::Anchor,
    )
    .map_err(|error| (preflight(), error))?;
    ensure_absent(new_key_path, "signing key").map_err(|error| (preflight(), error))?;
    ensure_absent(recovery_anchor_path, "recovery trusted head")
        .map_err(|error| (preflight(), error))?;

    let old_bytes = read_bounded_file(
        current_anchor_path,
        MAX_TRUSTED_HEAD_BYTES,
        FileKind::TrustedHead,
    )
    .map_err(|error| (preflight(), error))?;
    let new_key =
        ManifestSigningKey::generate().map_err(|_| (preflight(), TrustError::SigningFailed))?;
    let new_anchor =
        sign_fully_verified_head(library, &new_key).map_err(|error| (preflight(), error))?;
    let new_anchor_bytes = new_anchor
        .to_canonical_json()
        .map_err(|_| (preflight(), TrustError::InvalidTrustedHead))?;

    write_create_new(
        new_key_path,
        &new_key.to_pkcs8_bytes(),
        "creating rotated signing key",
    )
    .map_err(|error| (preflight(), error))?;
    let key_phase = RotationPhase::NewKeyDurable;
    write_create_new(
        recovery_anchor_path,
        &old_bytes,
        "creating recovery trusted head",
    )
    .map_err(|error| (key_phase, error))?;
    let recovery_phase = RotationPhase::RecoveryAnchorDurable;
    atomic_replace(current_anchor_path, &new_anchor_bytes).map_err(|failure| {
        let phase = if failure.renamed {
            RotationPhase::CurrentAnchorReplaced
        } else {
            recovery_phase
        };
        (phase, failure.error)
    })?;

    Ok(RotationSummary {
        previous: inspection.anchor,
        current: TrustedHeadSummary::from(&new_anchor),
    })
}

fn classify_report(report: &VerificationReport) -> AnchorComparison {
    if report.is_authenticated_against_trusted_head() {
        AnchorComparison::AuthenticatedCurrent
    } else if report
        .findings
        .iter()
        .any(|finding| finding.kind == VerificationFindingKind::TrustedHeadSignatureInvalid)
    {
        AnchorComparison::InvalidSignature
    } else if report
        .findings
        .iter()
        .any(|finding| finding.kind == VerificationFindingKind::TrustedHeadMismatch)
    {
        AnchorComparison::DifferentHead
    } else {
        AnchorComparison::LocalVerificationFailed
    }
}

fn sign_fully_verified_head(
    library: &Library,
    key: &ManifestSigningKey,
) -> Result<TrustedManifestHead, TrustError> {
    // Signing is kept in memory until a second operation fully verifies the exact
    // signed head. A concurrent manifest advance becomes a mismatch and therefore
    // fails before any external anchor is created or replaced.
    let anchor = sign_current_manifest_head(library, key).map_err(|_| TrustError::SigningFailed)?;
    let report = verify_library_against_trusted_head(
        library,
        VerificationOptions::new(VerificationScope::Full),
        &anchor,
    )
    .map_err(|_| TrustError::VerificationFailed)?;
    if !report.is_authenticated_against_trusted_head() {
        return Err(TrustError::LibraryNotFullyVerified {
            finding_count: report.finding_count,
        });
    }
    Ok(anchor)
}

#[derive(Clone, Copy)]
enum StorageSensitivity {
    Secret,
    Anchor,
}

#[derive(Clone, Copy)]
enum FileKind {
    SigningKey,
    TrustedHead,
}

fn validate_external_target(
    library_root: &Path,
    path: &Path,
    sensitivity: StorageSensitivity,
) -> Result<(), TrustError> {
    if !path.is_absolute() {
        return Err(TrustError::UnsafePath("path must be absolute"));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(TrustError::UnsafePath("path has no parent directory"))?;
    let parent_link = fs::symlink_metadata(parent)
        .map_err(|error| io_error("inspecting external parent directory", error))?;
    if parent_link.file_type().is_symlink() || !parent_link.is_dir() {
        return Err(TrustError::UnsafePath(
            "parent must be an existing non-symlink directory",
        ));
    }
    validate_private_parent(&parent_link, sensitivity)?;

    let canonical_library = fs::canonicalize(library_root)
        .map_err(|error| io_error("resolving library root", error))?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| io_error("resolving external parent directory", error))?;
    if canonical_parent.starts_with(&canonical_library) {
        return Err(TrustError::UnsafePath(
            "path must be outside the library tree",
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(TrustError::UnsafePath("target must not be a symlink"));
    }
    Ok(())
}

fn validate_private_parent(
    metadata: &fs::Metadata,
    _sensitivity: StorageSensitivity,
) -> Result<(), TrustError> {
    if metadata.uid() != current_effective_uid() {
        return Err(TrustError::InsecurePermissions(
            "parent directory is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(TrustError::InsecurePermissions(
            "parent directory must not grant group or other access",
        ));
    }
    if metadata.mode() & 0o300 != 0o300 {
        return Err(TrustError::InsecurePermissions(
            "parent directory must be writable and searchable by its owner",
        ));
    }
    Ok(())
}

fn current_effective_uid() -> u32 {
    Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(u32::MAX)
}

fn ensure_absent(path: &Path, kind: &'static str) -> Result<(), TrustError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(TrustError::AlreadyExists(kind)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("checking create-new target", error)),
    }
}

fn paths_alias(left: &Path, right: &Path) -> Result<bool, TrustError> {
    if left == right {
        return Ok(true);
    }
    let left_parent = left
        .parent()
        .ok_or(TrustError::UnsafePath("path has no parent directory"))?;
    let right_parent = right
        .parent()
        .ok_or(TrustError::UnsafePath("path has no parent directory"))?;
    let left_parent = fs::canonicalize(left_parent)
        .map_err(|error| io_error("resolving external parent directory", error))?;
    let right_parent = fs::canonicalize(right_parent)
        .map_err(|error| io_error("resolving external parent directory", error))?;
    if left_parent == right_parent && left.file_name() == right.file_name() {
        return Ok(true);
    }
    let left_metadata = fs::metadata(left);
    let right_metadata = fs::metadata(right);
    match (left_metadata, right_metadata) {
        (Ok(left), Ok(right)) => Ok(left.dev() == right.dev() && left.ino() == right.ino()),
        (Err(left), Err(right))
            if left.kind() == io::ErrorKind::NotFound
                && right.kind() == io::ErrorKind::NotFound =>
        {
            Ok(false)
        }
        (Err(error), _) | (_, Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        (Err(error), _) | (_, Err(error)) => Err(io_error("comparing external trust paths", error)),
    }
}

fn read_bounded_file(path: &Path, maximum: usize, kind: FileKind) -> Result<Vec<u8>, TrustError> {
    let before = fs::symlink_metadata(path)
        .map_err(|error| io_error("inspecting external trust file", error))?;
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(TrustError::UnsafePath(
            "external trust input must be a regular non-symlink file",
        ));
    }
    if before.uid() != current_effective_uid() {
        return Err(TrustError::InsecurePermissions(
            "external trust input is not owned by the current user",
        ));
    }
    if matches!(kind, FileKind::SigningKey) {
        let permissions = before.mode() & 0o777;
        if permissions & 0o177 != 0 || permissions & 0o400 == 0 {
            return Err(TrustError::InsecurePermissions(
                "signing key must have mode 0600 or read-only 0400",
            ));
        }
    }
    if before.len() > maximum as u64 {
        return Err(TrustError::InputTooLarge(match kind {
            FileKind::SigningKey => "signing key",
            FileKind::TrustedHead => "trusted head",
        }));
    }

    let mut file =
        File::open(path).map_err(|error| io_error("opening external trust file", error))?;
    let opened = file
        .metadata()
        .map_err(|error| io_error("inspecting opened external trust file", error))?;
    let after = fs::symlink_metadata(path)
        .map_err(|error| io_error("rechecking external trust file", error))?;
    if !opened.is_file()
        || after.file_type().is_symlink()
        || opened.dev() != before.dev()
        || opened.ino() != before.ino()
        || after.dev() != before.dev()
        || after.ino() != before.ino()
    {
        return Err(TrustError::UnsafePath(
            "external trust input changed while it was opened",
        ));
    }
    let read_bound = u64::try_from(maximum)
        .expect("trust input bounds fit u64")
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(before.len().min(maximum as u64) as usize);
    (&mut file)
        .take(read_bound)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error("reading bounded external trust file", error))?;
    if bytes.len() > maximum {
        return Err(TrustError::InputTooLarge(match kind {
            FileKind::SigningKey => "signing key",
            FileKind::TrustedHead => "trusted head",
        }));
    }
    Ok(bytes)
}

fn write_anchor(path: &Path, bytes: &[u8], mode: AnchorWriteMode) -> Result<(), TrustError> {
    match mode {
        AnchorWriteMode::CreateNew => {
            ensure_absent(path, "trusted head")?;
            write_create_new(path, bytes, "creating trusted head")
        }
        AnchorWriteMode::RefreshExisting => {
            let existing = read_bounded_file(path, MAX_TRUSTED_HEAD_BYTES, FileKind::TrustedHead)?;
            TrustedManifestHead::from_canonical_json(&existing)
                .map_err(|_| TrustError::InvalidTrustedHead)?;
            atomic_replace(path, bytes).map_err(|failure| failure.error)
        }
    }
}

fn write_create_new(path: &Path, bytes: &[u8], operation: &'static str) -> Result<(), TrustError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            TrustError::AlreadyExists("external trust file")
        } else {
            io_error(operation, error)
        }
    })?;
    let file_result = (|| {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error(operation, error))?;
        file.write_all(bytes)
            .map_err(|error| io_error(operation, error))?;
        file.sync_all().map_err(|error| io_error(operation, error))
    })();
    if file_result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
        return file_result;
    }
    sync_parent(path).map_err(|error| io_error(operation, error))
}

struct ReplaceFailure {
    renamed: bool,
    error: TrustError,
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), ReplaceFailure> {
    let parent = path.parent().expect("validated external path has a parent");
    let (temporary, mut file) = allocate_temporary(parent).map_err(|error| ReplaceFailure {
        renamed: false,
        error,
    })?;
    let result = (|| {
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error("restricting trusted-head staging file", error))?;
        file.write_all(bytes)
            .map_err(|error| io_error("writing trusted-head staging file", error))?;
        file.sync_all()
            .map_err(|error| io_error("syncing trusted-head staging file", error))?;
        fs::rename(&temporary, path)
            .map_err(|error| io_error("installing refreshed trusted head", error))?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(ReplaceFailure {
            renamed: false,
            error,
        });
    }
    sync_parent(path).map_err(|error| ReplaceFailure {
        renamed: true,
        error: io_error("syncing refreshed trusted-head directory", error),
    })
}

fn allocate_temporary(parent: &Path) -> Result<(PathBuf, File), TrustError> {
    for counter in 0..128_u32 {
        let candidate = parent.join(format!(
            ".wikisync-trusted-head-{}-{counter}.tmp",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error("allocating trusted-head staging path", error)),
        }
    }
    Err(TrustError::Io {
        operation: "allocating trusted-head staging path",
        kind: io::ErrorKind::AlreadyExists,
    })
}

fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(path.parent().expect("validated path has parent"))?.sync_all()
}

fn io_error(operation: &'static str, error: io::Error) -> TrustError {
    TrustError::Io {
        operation,
        kind: error.kind(),
    }
}
