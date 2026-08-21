//! Integrity verification for a WikiSyncer library.
//!
//! Verification establishes that canonical bytes still match the content-derived
//! identities recorded when they were captured. It does not establish that an
//! upstream statement is true, unbiased, complete, or still publicly available.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs;

use wikisync_store::{
    Library, ManifestId, ObjectId, ObjectVerificationState, StoreError, SyncRunState,
};

/// Default number of logical-object records loaded in one bounded store query.
pub const DEFAULT_PAGE_SIZE: u32 = 256;

/// Default maximum number of objects read by a quick verification.
pub const DEFAULT_QUICK_OBJECT_LIMIT: u64 = 100;

/// Default maximum number of detailed findings retained in memory.
pub const DEFAULT_MAX_RETAINED_FINDINGS: usize = 100;

/// Maximum page size supported by the store enumeration contract.
pub const MAX_PAGE_SIZE: u32 = 1_000;

/// Amount of the logical object catalog to read and hash-check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationScope {
    /// Check a deterministic, bounded prefix of the logical object catalog.
    ///
    /// A quick check reports partial coverage when the library contains more than
    /// the configured quick-object limit. It never claims whole-library integrity
    /// in that case.
    Quick,
    /// Read and hash-check every logical object visible during the scan.
    Full,
}

/// Resource bounds for one verification operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerificationOptions {
    /// Requested verification coverage.
    pub scope: VerificationScope,
    /// Logical-object metadata records requested from the store at once.
    pub page_size: u32,
    /// Maximum objects read for [`VerificationScope::Quick`].
    pub quick_object_limit: u64,
    /// Maximum detailed findings retained in the report.
    ///
    /// The total finding count remains accurate after this limit is reached.
    pub max_retained_findings: usize,
}

impl VerificationOptions {
    /// Returns default bounded options for `scope`.
    #[must_use]
    pub const fn new(scope: VerificationScope) -> Self {
        Self {
            scope,
            page_size: DEFAULT_PAGE_SIZE,
            quick_object_limit: DEFAULT_QUICK_OBJECT_LIMIT,
            max_retained_findings: DEFAULT_MAX_RETAINED_FINDINGS,
        }
    }
}

/// Whether all logical objects in the observed catalog were examined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationCoverage {
    /// Every object in a stable observed catalog was examined.
    Complete,
    /// Verification intentionally or operationally covered only part of the catalog.
    Partial,
}

/// Stable category for one verification finding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationFindingKind {
    /// Persisted metadata did not describe the object as verified.
    MetadataNotVerified,
    /// The store could not reconstruct and hash-verify the canonical bytes.
    ObjectUnreadable,
    /// Returned canonical bytes disagreed with persisted logical length metadata.
    LengthMismatch,
    /// The logical object catalog changed while it was being checked.
    LibraryChangedDuringVerification,
    /// The manifest directory contained an unreadable or invalid entry.
    ManifestInventoryInvalid,
    /// An expected append sequence was absent from the manifest directory.
    ManifestMissing,
    /// A manifest failed canonical JSON, filename, bound, or body-identity validation.
    ManifestUnreadable,
    /// The first/predecessor identity did not form the required append chain.
    ManifestPredecessorMismatch,
    /// More than one manifest claimed the same durable synchronization run.
    DuplicateManifestRun,
    /// A manifest referred to a run that was absent or had not succeeded.
    ManifestRunNotSucceeded,
    /// A successful durable sync run had no installed manifest.
    SuccessfulRunMissingManifest,
    /// Manifest directory membership changed during the scan.
    ManifestsChangedDuringVerification,
}

/// One structured integrity finding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationFinding {
    /// Machine-matchable finding category.
    pub kind: VerificationFindingKind,
    /// Affected logical object, or `None` for a library-level finding.
    pub object_id: Option<ObjectId>,
    /// Affected manifest sequence, or `None` when no valid sequence is available.
    pub manifest_sequence: Option<u64>,
    /// Human-readable local diagnostic detail.
    pub message: String,
}

/// Result of one bounded integrity operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationReport {
    /// Requested scope.
    pub scope: VerificationScope,
    /// Whether every object in a stable observed catalog was examined.
    pub coverage: VerificationCoverage,
    /// Logical objects recorded when verification began.
    pub objects_at_start: u64,
    /// Logical objects recorded when verification ended.
    pub objects_at_end: u64,
    /// Logical-object metadata records examined.
    pub objects_examined: u64,
    /// Objects whose canonical bytes were successfully reconstructed and hash-checked.
    pub objects_verified: u64,
    /// Canonical uncompressed bytes successfully verified.
    pub canonical_bytes_verified: u64,
    /// Canonically named manifest files observed when manifest verification began.
    pub manifests_at_start: u64,
    /// Canonically named manifest files observed when manifest verification ended.
    pub manifests_at_end: u64,
    /// Manifest files whose bounded canonical representation was examined.
    pub manifests_examined: u64,
    /// Manifest files whose embedded identity reproduced their canonical body.
    pub manifests_identity_verified: u64,
    /// Total findings, including details omitted by the report bound.
    pub finding_count: u64,
    /// First bounded set of structured findings.
    pub findings: Vec<VerificationFinding>,
    /// Findings omitted after `max_retained_findings` was reached.
    pub omitted_findings: u64,
}

impl VerificationReport {
    /// Returns whether the report verifies every object in the stable observed
    /// library catalog since capture.
    ///
    /// This is strictly an integrity statement about captured bytes. It is not a
    /// statement about the truth or continued upstream availability of their content.
    #[must_use]
    pub const fn is_verified_since_capture(&self) -> bool {
        matches!(self.coverage, VerificationCoverage::Complete)
            && self.finding_count == 0
            && self.objects_examined == self.objects_at_start
            && self.objects_verified == self.objects_at_start
            && self.objects_at_start == self.objects_at_end
    }
}

/// Verifies a library with the default bounds for `scope`.
pub fn verify_library(
    library: &Library,
    scope: VerificationScope,
) -> Result<VerificationReport, VerificationError> {
    verify_library_with_options(library, VerificationOptions::new(scope))
}

/// Verifies a library with explicit query, quick-scan, and report bounds.
pub fn verify_library_with_options(
    library: &Library,
    options: VerificationOptions,
) -> Result<VerificationReport, VerificationError> {
    validate_options(options)?;

    let objects_at_start = library.logical_object_count()?;
    let target = match options.scope {
        VerificationScope::Quick => objects_at_start.min(options.quick_object_limit),
        VerificationScope::Full => objects_at_start,
    };
    let mut report = VerificationReport {
        scope: options.scope,
        coverage: if target == objects_at_start {
            VerificationCoverage::Complete
        } else {
            VerificationCoverage::Partial
        },
        objects_at_start,
        objects_at_end: objects_at_start,
        objects_examined: 0,
        objects_verified: 0,
        canonical_bytes_verified: 0,
        manifests_at_start: 0,
        manifests_at_end: 0,
        manifests_examined: 0,
        manifests_identity_verified: 0,
        finding_count: 0,
        findings: Vec::new(),
        omitted_findings: 0,
    };
    let mut cursor = None;

    while report.objects_examined < target {
        let remaining = target - report.objects_examined;
        let limit = u32::try_from(u64::from(options.page_size).min(remaining))
            .expect("verification page limit is already bounded to u32");
        let page = library.logical_objects_after(cursor, limit)?;
        if page.is_empty() {
            report.coverage = VerificationCoverage::Partial;
            break;
        }

        for logical in page {
            let object_id = logical.object.id;
            if cursor.is_some_and(|previous| object_id <= previous) {
                return Err(VerificationError::NonAdvancingObjectPage {
                    previous: cursor,
                    current: object_id,
                });
            }
            cursor = Some(object_id);
            report.objects_examined += 1;

            if logical.verification_state != ObjectVerificationState::Verified {
                push_finding(
                    &mut report,
                    options.max_retained_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::MetadataNotVerified,
                        object_id: Some(object_id),
                        manifest_sequence: None,
                        message: format!(
                            "logical object metadata state is {:?}, not verified",
                            logical.verification_state
                        ),
                    },
                );
            }

            match library.read_object(object_id) {
                Ok(bytes) => {
                    let actual_length = bytes.len() as u64;
                    if actual_length != logical.object.uncompressed_length {
                        push_finding(
                            &mut report,
                            options.max_retained_findings,
                            VerificationFinding {
                                kind: VerificationFindingKind::LengthMismatch,
                                object_id: Some(object_id),
                                manifest_sequence: None,
                                message: format!(
                                    "verified read returned {actual_length} bytes; metadata records {}",
                                    logical.object.uncompressed_length
                                ),
                            },
                        );
                    } else {
                        report.objects_verified += 1;
                        report.canonical_bytes_verified = report
                            .canonical_bytes_verified
                            .checked_add(actual_length)
                            .ok_or(VerificationError::CounterOverflow(
                                "canonical bytes verified",
                            ))?;
                    }
                }
                Err(error) => push_finding(
                    &mut report,
                    options.max_retained_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::ObjectUnreadable,
                        object_id: Some(object_id),
                        manifest_sequence: None,
                        message: error.to_string(),
                    },
                ),
            }
        }
    }

    report.objects_at_end = library.logical_object_count()?;
    if report.objects_at_end != report.objects_at_start {
        report.coverage = VerificationCoverage::Partial;
        let message = format!(
            "logical object count changed from {} to {} during verification",
            report.objects_at_start, report.objects_at_end
        );
        push_finding(
            &mut report,
            options.max_retained_findings,
            VerificationFinding {
                kind: VerificationFindingKind::LibraryChangedDuringVerification,
                object_id: None,
                manifest_sequence: None,
                message,
            },
        );
    }
    if report.objects_examined != target {
        report.coverage = VerificationCoverage::Partial;
    }
    if options.scope == VerificationScope::Full {
        verify_manifest_history(library, options.max_retained_findings, &mut report)?;
    }

    Ok(report)
}

fn verify_manifest_history(
    library: &Library,
    maximum_findings: usize,
    report: &mut VerificationReport,
) -> Result<(), VerificationError> {
    let start_names = match manifest_inventory_names(library) {
        Ok(names) => names,
        Err(error) => {
            report.coverage = VerificationCoverage::Partial;
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestInventoryInvalid,
                    object_id: None,
                    manifest_sequence: None,
                    message: error.to_string(),
                },
            );
            return Ok(());
        }
    };
    let mut sequences = Vec::new();
    for name in &start_names {
        let Some(name) = name.to_str() else {
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestInventoryInvalid,
                    object_id: None,
                    manifest_sequence: None,
                    message: "manifest filename is not UTF-8".to_owned(),
                },
            );
            continue;
        };
        match parse_manifest_sequence(name) {
            Some(sequence) => sequences.push(sequence),
            None => push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestInventoryInvalid,
                    object_id: None,
                    manifest_sequence: None,
                    message: format!("unexpected manifest directory entry {name:?}"),
                },
            ),
        }
    }
    sequences.sort_unstable();
    report.manifests_at_start = sequences.len() as u64;

    let mut expected_sequence = 1_u64;
    let mut previous: Option<(u64, ManifestId)> = None;
    let mut represented_runs = HashSet::new();
    for sequence in sequences {
        if sequence != expected_sequence {
            let message = if sequence > expected_sequence {
                format!(
                    "manifest sequence {expected_sequence} through {} is missing before {sequence}",
                    sequence - 1
                )
            } else {
                format!("manifest sequence {sequence} is duplicated or reordered")
            };
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestMissing,
                    object_id: None,
                    manifest_sequence: Some(expected_sequence),
                    message,
                },
            );
        }
        expected_sequence = sequence.saturating_add(1);
        report.manifests_examined = report.manifests_examined.saturating_add(1);
        let stored = match library.read_manifest(sequence) {
            Ok(stored) => stored,
            Err(error) => {
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::ManifestUnreadable,
                        object_id: None,
                        manifest_sequence: Some(sequence),
                        message: error.to_string(),
                    },
                );
                previous = None;
                continue;
            }
        };
        report.manifests_identity_verified = report.manifests_identity_verified.saturating_add(1);

        let expected_predecessor = if sequence == 1 {
            None
        } else {
            previous
                .filter(|(previous_sequence, _)| *previous_sequence + 1 == sequence)
                .map(|(_, id)| id)
        };
        let chain_matches = if sequence == 1 {
            stored.manifest.predecessor.is_none()
        } else {
            expected_predecessor.is_some() && stored.manifest.predecessor == expected_predecessor
        };
        if !chain_matches {
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestPredecessorMismatch,
                    object_id: None,
                    manifest_sequence: Some(sequence),
                    message: format!(
                        "manifest predecessor {:?} does not match prior verified identity {:?}",
                        stored.manifest.predecessor, expected_predecessor
                    ),
                },
            );
        }
        previous = Some((sequence, stored.id));

        if !represented_runs.insert(stored.manifest.run_id) {
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::DuplicateManifestRun,
                    object_id: None,
                    manifest_sequence: Some(sequence),
                    message: format!(
                        "sync run {} is represented by more than one manifest",
                        stored.manifest.run_id
                    ),
                },
            );
        }
        let status = library.sync_run_status(stored.manifest.run_id)?;
        if status.is_none_or(|status| status.state != SyncRunState::Succeeded) {
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestRunNotSucceeded,
                    object_id: None,
                    manifest_sequence: Some(sequence),
                    message: format!(
                        "manifest refers to absent or unsuccessful sync run {}",
                        stored.manifest.run_id
                    ),
                },
            );
        }
    }

    let mut run_cursor = None;
    loop {
        let run_ids = library.succeeded_sync_run_ids_after(run_cursor, 1_000)?;
        if run_ids.is_empty() {
            break;
        }
        for run_id in &run_ids {
            if !represented_runs.contains(run_id) {
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::SuccessfulRunMissingManifest,
                        object_id: None,
                        manifest_sequence: None,
                        message: format!("successful sync run {run_id} has no manifest"),
                    },
                );
            }
        }
        run_cursor = run_ids.last().copied();
        if run_ids.len() < 1_000 {
            break;
        }
    }

    match manifest_inventory_names(library) {
        Ok(end_names) => {
            report.manifests_at_end = end_names
                .iter()
                .filter(|name| name.to_str().and_then(parse_manifest_sequence).is_some())
                .count() as u64;
            if end_names != start_names {
                report.coverage = VerificationCoverage::Partial;
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::ManifestsChangedDuringVerification,
                        object_id: None,
                        manifest_sequence: None,
                        message: "manifest directory changed during verification".to_owned(),
                    },
                );
            }
        }
        Err(error) => {
            report.coverage = VerificationCoverage::Partial;
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestsChangedDuringVerification,
                    object_id: None,
                    manifest_sequence: None,
                    message: error.to_string(),
                },
            );
        }
    }
    Ok(())
}

fn manifest_inventory_names(library: &Library) -> Result<Vec<std::ffi::OsString>, std::io::Error> {
    let mut names = fs::read_dir(library.root().join("manifests"))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

fn parse_manifest_sequence(name: &str) -> Option<u64> {
    if name.len() != 17 || !name.ends_with(".json") {
        return None;
    }
    let digits = &name[..12];
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let sequence = digits.parse().ok()?;
    (sequence > 0).then_some(sequence)
}

fn validate_options(options: VerificationOptions) -> Result<(), VerificationError> {
    if !(1..=MAX_PAGE_SIZE).contains(&options.page_size) {
        return Err(VerificationError::InvalidPageSize(options.page_size));
    }
    if options.quick_object_limit == 0 {
        return Err(VerificationError::ZeroQuickObjectLimit);
    }
    if options.max_retained_findings == 0 {
        return Err(VerificationError::ZeroFindingLimit);
    }
    Ok(())
}

fn push_finding(report: &mut VerificationReport, maximum: usize, finding: VerificationFinding) {
    report.finding_count = report.finding_count.saturating_add(1);
    if report.findings.len() < maximum {
        report.findings.push(finding);
    } else {
        report.omitted_findings = report.omitted_findings.saturating_add(1);
    }
}

/// Failure to configure or enumerate an integrity verification.
#[derive(Debug)]
pub enum VerificationError {
    /// Store metadata could not be enumerated.
    Store(StoreError),
    /// Page size was outside the public store bound.
    InvalidPageSize(u32),
    /// A quick scan must check at least one logical object when any exist.
    ZeroQuickObjectLimit,
    /// At least one detailed finding must be retained.
    ZeroFindingLimit,
    /// Store pagination violated its strict object-ID ordering contract.
    NonAdvancingObjectPage {
        /// Previously returned object, absent only before the first result.
        previous: Option<ObjectId>,
        /// Non-advancing object returned by the next page.
        current: ObjectId,
    },
    /// An exact aggregate could not be represented as a `u64`.
    CounterOverflow(&'static str),
}

impl fmt::Display for VerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "integrity metadata read failed: {error}"),
            Self::InvalidPageSize(size) => write!(
                formatter,
                "verification page size must be between 1 and {MAX_PAGE_SIZE}, got {size}"
            ),
            Self::ZeroQuickObjectLimit => {
                formatter.write_str("quick verification object limit must be greater than zero")
            }
            Self::ZeroFindingLimit => {
                formatter.write_str("verification finding limit must be greater than zero")
            }
            Self::NonAdvancingObjectPage { previous, current } => write!(
                formatter,
                "logical object pagination did not advance after {previous:?}: returned {current}"
            ),
            Self::CounterOverflow(counter) => {
                write!(formatter, "verification {counter} counter overflowed")
            }
        }
    }
}

impl Error for VerificationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl From<StoreError> for VerificationError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use wikisync_store::{Library, ObjectKind, SyncRunKind};

    use super::*;

    fn populated_library(count: usize) -> (TempDir, Library, u64) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let mut bytes = 0_u64;
        for index in 0..count {
            let source = format!("canonical fixture object {index}");
            bytes += source.len() as u64;
            library
                .put_bytes(ObjectKind::Wikitext, source.as_bytes())
                .expect("store object");
        }
        (directory, library, bytes)
    }

    fn manifested_library(count: usize) -> (TempDir, Library) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        for index in 0..count {
            let candidate = 100 + index as u64;
            let run_id = library
                .start_or_resume_sync_run(
                    wiki_id,
                    None,
                    if index == 0 {
                        SyncRunKind::Bootstrap
                    } else {
                        SyncRunKind::Update
                    },
                    candidate,
                )
                .expect("start run")
                .status
                .run_id;
            library
                .complete_sync_run(run_id, None)
                .expect("complete run");
            library.append_sync_manifest(run_id).expect("manifest run");
        }
        (directory, library)
    }

    #[test]
    fn full_verification_reads_every_object_across_bounded_pages() {
        let (_directory, library, expected_bytes) = populated_library(7);
        let options = VerificationOptions {
            page_size: 2,
            ..VerificationOptions::new(VerificationScope::Full)
        };

        let report = verify_library_with_options(&library, options).expect("verification");

        assert_eq!(report.coverage, VerificationCoverage::Complete);
        assert_eq!(report.objects_at_start, 7);
        assert_eq!(report.objects_at_end, 7);
        assert_eq!(report.objects_examined, 7);
        assert_eq!(report.objects_verified, 7);
        assert_eq!(report.canonical_bytes_verified, expected_bytes);
        assert_eq!(report.finding_count, 0);
        assert!(report.is_verified_since_capture());
    }

    #[test]
    fn quick_verification_never_claims_unchecked_objects_are_verified() {
        let (_directory, library, _bytes) = populated_library(5);
        let options = VerificationOptions {
            page_size: 1,
            quick_object_limit: 2,
            ..VerificationOptions::new(VerificationScope::Quick)
        };

        let report = verify_library_with_options(&library, options).expect("verification");

        assert_eq!(report.coverage, VerificationCoverage::Partial);
        assert_eq!(report.objects_at_start, 5);
        assert_eq!(report.objects_examined, 2);
        assert_eq!(report.objects_verified, 2);
        assert_eq!(report.finding_count, 0);
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn quick_verification_can_cover_a_small_library_completely() {
        let (_directory, library, _bytes) = populated_library(2);

        let report = verify_library(&library, VerificationScope::Quick).expect("verification");

        assert_eq!(report.coverage, VerificationCoverage::Complete);
        assert_eq!(report.objects_verified, 2);
        assert!(report.is_verified_since_capture());
    }

    #[test]
    fn tampered_loose_content_is_a_structured_integrity_finding() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let stored = library
            .put_bytes(ObjectKind::Wikitext, b"canonical bytes")
            .expect("store object");
        let encoded = stored.id.to_string();
        let digest = encoded.strip_prefix("b3:").expect("object prefix");
        let loose_path = directory
            .path()
            .join("objects/loose/b3")
            .join(&digest[..2])
            .join(&digest[2..4])
            .join(digest);
        fs::write(&loose_path, b"tampered compressed bytes").expect("tamper loose object");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(report.coverage, VerificationCoverage::Complete);
        assert_eq!(report.objects_examined, 1);
        assert_eq!(report.objects_verified, 0);
        assert_eq!(report.finding_count, 1);
        assert_eq!(
            report.findings[0].kind,
            VerificationFindingKind::ObjectUnreadable
        );
        assert_eq!(report.findings[0].object_id, Some(stored.id));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn finding_details_are_bounded_without_losing_the_total() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        for index in 0..3 {
            let stored = library
                .put_bytes(ObjectKind::Wikitext, format!("object {index}").as_bytes())
                .expect("store object");
            let encoded = stored.id.to_string();
            let digest = encoded.strip_prefix("b3:").expect("object prefix");
            let loose_path = directory
                .path()
                .join("objects/loose/b3")
                .join(&digest[..2])
                .join(&digest[2..4])
                .join(digest);
            fs::write(loose_path, b"broken").expect("tamper object");
        }
        let options = VerificationOptions {
            page_size: 1,
            max_retained_findings: 1,
            ..VerificationOptions::new(VerificationScope::Full)
        };

        let report = verify_library_with_options(&library, options).expect("verification");

        assert_eq!(report.finding_count, 3);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.omitted_findings, 2);
    }

    #[test]
    fn full_verification_checks_manifest_identity_chain_and_successful_runs() {
        let (_directory, library) = manifested_library(3);

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(report.coverage, VerificationCoverage::Complete);
        assert_eq!(report.manifests_at_start, 3);
        assert_eq!(report.manifests_at_end, 3);
        assert_eq!(report.manifests_examined, 3);
        assert_eq!(report.manifests_identity_verified, 3);
        assert_eq!(report.finding_count, 0);
        assert!(report.is_verified_since_capture());
    }

    #[test]
    fn tampered_manifest_identity_is_a_structured_finding() {
        let (directory, library) = manifested_library(1);
        let path = directory.path().join("manifests/000000000001.json");
        let original = fs::read_to_string(&path).expect("manifest");
        let tampered = original.replace(
            "\"capture_completed_at\":100",
            "\"capture_completed_at\":101",
        );
        assert_ne!(tampered, original);
        fs::write(path, tampered).expect("tamper manifest");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestUnreadable
                && finding.manifest_sequence == Some(1)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn missing_manifest_history_and_successful_run_gap_are_structured_findings() {
        let (directory, library) = manifested_library(3);
        fs::remove_file(directory.path().join("manifests/000000000002.json"))
            .expect("remove middle manifest");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestMissing
                && finding.manifest_sequence == Some(2)
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestPredecessorMismatch
                && finding.manifest_sequence == Some(3)
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::SuccessfulRunMissingManifest
        }));
    }

    #[test]
    fn swapped_manifest_files_are_detected_as_reordered_history() {
        let (directory, library) = manifested_library(2);
        let first_path = directory.path().join("manifests/000000000001.json");
        let second_path = directory.path().join("manifests/000000000002.json");
        let first = fs::read(&first_path).expect("first");
        let second = fs::read(&second_path).expect("second");
        fs::write(&first_path, second).expect("swap first");
        fs::write(&second_path, first).expect("swap second");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        let unreadable = report
            .findings
            .iter()
            .filter(|finding| finding.kind == VerificationFindingKind::ManifestUnreadable)
            .count();
        assert_eq!(unreadable, 2);
        assert_eq!(report.manifests_identity_verified, 0);
    }

    #[test]
    fn completed_run_without_manifest_is_detected_and_locally_repairable() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start")
            .status
            .run_id;
        library.complete_sync_run(run_id, None).expect("complete");

        let before = verify_library(&library, VerificationScope::Full).expect("before repair");
        assert!(before.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::SuccessfulRunMissingManifest
        }));

        library
            .append_missing_sync_manifests(1)
            .expect("repair manifest");
        let after = verify_library(&library, VerificationScope::Full).expect("after repair");
        assert_eq!(after.finding_count, 0);
        assert_eq!(after.manifests_identity_verified, 1);
    }

    #[test]
    fn options_reject_unbounded_or_empty_limits() {
        let (_directory, library, _bytes) = populated_library(1);
        let too_large = VerificationOptions {
            page_size: MAX_PAGE_SIZE + 1,
            ..VerificationOptions::new(VerificationScope::Full)
        };
        assert!(matches!(
            verify_library_with_options(&library, too_large),
            Err(VerificationError::InvalidPageSize(_))
        ));

        let no_findings = VerificationOptions {
            max_retained_findings: 0,
            ..VerificationOptions::new(VerificationScope::Full)
        };
        assert!(matches!(
            verify_library_with_options(&library, no_findings),
            Err(VerificationError::ZeroFindingLimit)
        ));
    }
}
