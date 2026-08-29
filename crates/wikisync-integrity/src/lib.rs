//! Integrity verification for a WikiSyncer library.
//!
//! Verification establishes that canonical bytes still match the content-derived
//! identities recorded when they were captured. It does not establish that an
//! upstream statement is true, unbiased, complete, or still publicly available.
//! Full verification also checks reference consistency exposed by the current
//! schema. Schema version 15 has no persistent derived-cache table, so no report from
//! this version claims derived-cache inventory or cache-body verification.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;

use ring::rand::SystemRandom;
use ring::signature::{self, Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use wikisync_content::{PLAIN_TEXT_TRANSFORMER_VERSION, ThumbnailLimits, validate_thumbnail};
use wikisync_core::{MAX_THUMBNAIL_BYTES, MAX_THUMBNAIL_EDGE_PIXELS};
use wikisync_store::{
    IntegrityMetadataIssue, IntegrityMetadataSubject, Library, ManifestId, ManifestPageHead,
    ManifestRevision, ManifestShard, ManifestShardKind, ObjectId, ObjectVerificationState,
    PurgeJournalState, StoreError, StoredManifest, SyncRunState,
};

const TRUSTED_HEAD_SCHEMA_VERSION: u32 = 1;
const TRUSTED_HEAD_ALGORITHM: &str = "Ed25519";
const TRUSTED_HEAD_DOMAIN: &[u8] = b"wikisync-trusted-manifest-head-v1\0";
const MAX_THUMBNAIL_BYTES_PER_PIXEL: u64 = 8;

/// Byte length of an Ed25519 public verification key.
pub const ED25519_PUBLIC_KEY_BYTES: usize = 32;

/// Byte length of an Ed25519 signature.
pub const ED25519_SIGNATURE_BYTES: usize = 64;

/// Maximum accepted canonical trusted-head document size.
pub const MAX_TRUSTED_HEAD_BYTES: usize = 4 * 1024;

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

/// Coverage of media inventory by the newest manifest for each represented sync
/// scope. Older schema-v1 manifests remain readable but do not authenticate media.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestMediaCoverage {
    /// No represented scope has a media-aware manifest boundary.
    NotCovered,
    /// Some, but not every, represented scope has a media-aware latest boundary.
    Partial,
    /// Every represented scope has a media-aware latest boundary.
    Complete,
}

/// Stable category for one verification finding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
    /// A manifested revision claim no longer had a retained revision record.
    ManifestRevisionClaimMissing,
    /// A manifested revision claim's retained owning page was absent.
    ManifestRevisionClaimPageMissing,
    /// A manifested revision claim now belonged to a different page.
    ManifestRevisionClaimPageMismatch,
    /// A manifested revision claim now selected a different canonical object.
    ManifestRevisionClaimObjectMismatch,
    /// A manifested positive page-head claim's retained page was absent.
    ManifestPageHeadClaimPageMissing,
    /// A manifested positive page-head claim's retained revision was absent.
    ManifestPageHeadClaimRevisionMissing,
    /// A manifested positive page-head revision now belonged to a different page.
    ManifestPageHeadClaimRevisionPageMismatch,
    /// Retained metadata changed while historical manifest claims were replayed.
    ManifestClaimsChangedDuringVerification,
    /// Manifest directory membership changed during the scan.
    ManifestsChangedDuringVerification,
    /// Media recorded at the authenticated run boundary is no longer inventoried.
    ManifestMediaDeleted,
    /// Durable media metadata no longer reproduces its authenticated identity.
    ManifestMediaTampered,
    /// A recorded revision media placement is no longer inventoried.
    ManifestMediaPlacementDeleted,
    /// A placement now selects a different media rendition than the manifest records.
    ManifestMediaPlacementSwapped,
    /// Placement display metadata no longer reproduces its authenticated identity.
    ManifestMediaPlacementTampered,
    /// The current scope contains media or placements absent from its latest manifest.
    ManifestMediaInventoryChanged,
    /// SQLite media inventory changed while manifest media was being compared.
    ManifestMediaInventoryChangedDuringVerification,
    /// An authenticated purge event had no corresponding durable cleanup journal.
    PurgeJournalMissing,
    /// A durable cleanup journal did not exactly match its authenticated purge event.
    PurgeJournalMismatch,
    /// The durable purge-object inventory did not reproduce its authenticated identity.
    PurgeInventoryMismatch,
    /// Authorized-absence, physical-work, or completion-accounting state disagreed
    /// with the authenticated purge event and cleanup phase.
    PurgeCleanupMismatch,
    /// A purge inventory object is still required by a retained reference outside
    /// the authorized collection closure.
    PurgeSharedReferenceViolation,
    /// Canonical payload was absent without an exact authenticated purge authorization.
    UnexplainedObjectLoss,
    /// The external trusted-head signature did not verify with its embedded key.
    TrustedHeadSignatureInvalid,
    /// The authenticated external head did not match the current local manifest head.
    TrustedHeadMismatch,
    /// A revision pointed to an absent owning page.
    RevisionPageUnreachable,
    /// A revision pointed to an absent canonical content object.
    RevisionObjectUnreachable,
    /// A locally captured parent revision belonged to another page.
    RevisionParentPageMismatch,
    /// A revision directly identified itself as its parent.
    RevisionParentSelfReference,
    /// A page head pointed to an absent revision.
    PageHeadRevisionUnreachable,
    /// A page head revision belonged to another page.
    PageHeadRevisionPageMismatch,
    /// A checkpoint's collection belonged to another wiki or was absent.
    CheckpointCollectionMismatch,
    /// A committed checkpoint had no reachable advancing run.
    CheckpointRunUnreachable,
    /// A checkpoint referred to a run that had not succeeded.
    CheckpointRunNotSucceeded,
    /// A checkpoint and its advancing run had different wiki/collection scope.
    CheckpointRunScopeMismatch,
    /// A checkpoint boundary disagreed with its advancing run candidate.
    CheckpointBoundaryMismatch,
    /// A search document pointed to an absent page.
    SearchPageUnreachable,
    /// A search document pointed to an absent revision.
    SearchRevisionUnreachable,
    /// A search document's revision belonged to another page.
    SearchRevisionPageMismatch,
    /// A search document did not identify the page's current revision.
    SearchRevisionNotCurrent,
    /// A search metadata row had no corresponding contentless FTS row.
    SearchFtsRowMissing,
    /// A contentless FTS row had no corresponding search metadata row.
    SearchFtsRowOrphan,
    /// Captured media metadata pointed to an absent canonical object.
    MediaObjectUnreachable,
    /// Captured media metadata pointed to a logical object that was not media.
    MediaObjectKindMismatch,
    /// Captured media metadata or its bounded passive-raster signature was inconsistent.
    MediaMetadataMismatch,
    /// A media-linked canonical object could not be reconstructed and hash-verified.
    MediaObjectUnreadable,
    /// A media placement pointed to an absent revision in its recorded wiki.
    MediaPlacementRevisionUnreachable,
    /// A media placement's revision pointed to an absent owning page.
    MediaPlacementPageUnreachable,
    /// A media placement pointed to absent media metadata in its recorded wiki.
    MediaPlacementMediaUnreachable,
    /// A media placement contained invalid bounded placement metadata.
    MediaPlacementMetadataMismatch,
    /// A search document was produced by a transformer other than the current one.
    SearchTransformerVersionMismatch,
    /// The metadata-reference catalog changed during the scan.
    MetadataChangedDuringVerification,
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
    /// Affected metadata record, or `None` for object/manifest/library findings.
    pub metadata_subject: Option<IntegrityMetadataSubject>,
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
    /// Historical introduced-revision claims compared with retained metadata.
    pub manifest_revision_claims_examined: u64,
    /// Historical positive page-head claims compared with retained metadata.
    ///
    /// Page-head entries without a revision do not make a retained revision claim
    /// and are therefore excluded from this counter.
    pub manifest_page_head_claims_examined: u64,
    /// Whether the newest manifest for each represented scope authenticates media.
    pub manifest_media_coverage: ManifestMediaCoverage,
    /// Latest media-aware scope snapshots compared with current durable metadata.
    pub manifest_media_snapshots_examined: u64,
    /// Authenticated purge events compared with their exact durable journals.
    pub purge_events_examined: u64,
    /// Valid authenticated purge journals whose cleanup is not yet complete.
    pub purges_pending_cleanup: u64,
    /// Absent canonical objects exactly explained by authenticated purges in cleaning
    /// or succeeded state, positive per-object absence records, and verified cleanup
    /// accounting. An unreadable retained object is never inferred to be purged.
    pub authorized_absences_verified: u64,
    /// Revision, page, checkpoint, search-document, FTS, media, and media-placement
    /// records present when a full metadata-reference scan began. Quick verification
    /// leaves this zero.
    pub metadata_records_at_start: u64,
    /// Metadata-reference records present when a full scan ended.
    pub metadata_records_at_end: u64,
    /// Metadata-reference records examined through bounded keyset pages, including
    /// media metadata and revision placements in a full scan.
    pub metadata_records_examined: u64,
    /// Whether an externally supplied Ed25519 trusted head was signature-verified
    /// and matched the current local manifest head.
    ///
    /// Unsigned verification leaves this false without adding a finding.
    pub trusted_head_authenticated: bool,
    /// Total findings, including details omitted by the report bound.
    pub finding_count: u64,
    /// First bounded set of structured findings.
    pub findings: Vec<VerificationFinding>,
    /// Findings omitted after `max_retained_findings` was reached.
    pub omitted_findings: u64,
}

impl VerificationReport {
    /// Returns whether the report verifies every retained object in the stable
    /// observed library catalog since capture and exactly authenticates every
    /// authorized absence.
    ///
    /// This is strictly an integrity statement about retained captured bytes and
    /// authenticated local purge evidence. It is not a statement about source truth,
    /// upstream availability, external copies, or physical secure erasure.
    #[must_use]
    pub const fn is_verified_since_capture(&self) -> bool {
        matches!(self.coverage, VerificationCoverage::Complete)
            && self.finding_count == 0
            && self.objects_examined == self.objects_at_start
            && self
                .objects_verified
                .saturating_add(self.authorized_absences_verified)
                == self.objects_at_start
            && self.objects_at_start == self.objects_at_end
            && (matches!(self.scope, VerificationScope::Quick)
                || (self.metadata_records_examined == self.metadata_records_at_start
                    && self.metadata_records_at_start == self.metadata_records_at_end))
    }

    /// Returns whether complete local integrity checks also matched an externally
    /// supplied, valid Ed25519 trusted head.
    ///
    /// This authenticates the observed manifest-chain head against the key embedded
    /// in that separately retained anchor. It does not establish source truth, and
    /// it cannot detect replacement of both the library and the external anchor.
    #[must_use]
    pub const fn is_authenticated_against_trusted_head(&self) -> bool {
        self.is_verified_since_capture() && self.trusted_head_authenticated
    }
}

/// Secret Ed25519 key material used only to sign exportable manifest-chain heads.
///
/// The PKCS#8 bytes are deliberately not exposed through `Debug`. Key persistence,
/// backup, and user-presence policy remain caller responsibilities.
pub struct ManifestSigningKey {
    pkcs8: Vec<u8>,
}

impl ManifestSigningKey {
    /// Generates a new Ed25519 signing key with the operating system random source.
    pub fn generate() -> Result<Self, TrustedHeadError> {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| TrustedHeadError::KeyGeneration)?;
        Ok(Self {
            pkcs8: document.as_ref().to_vec(),
        })
    }

    /// Imports and validates an Ed25519 PKCS#8 v2 key document.
    pub fn from_pkcs8(bytes: &[u8]) -> Result<Self, TrustedHeadError> {
        Ed25519KeyPair::from_pkcs8(bytes).map_err(|_| TrustedHeadError::InvalidSigningKey)?;
        Ok(Self {
            pkcs8: bytes.to_vec(),
        })
    }

    /// Returns the PKCS#8 key document for explicit caller-managed secret backup.
    ///
    /// Callers must store these bytes as sensitive key material; the trusted-head
    /// JSON contains only the public key and is safe to disclose.
    #[must_use]
    pub fn to_pkcs8_bytes(&self) -> Vec<u8> {
        self.pkcs8.clone()
    }

    fn key_pair(&self) -> Result<Ed25519KeyPair, TrustedHeadError> {
        Ed25519KeyPair::from_pkcs8(&self.pkcs8).map_err(|_| TrustedHeadError::InvalidSigningKey)
    }
}

impl fmt::Debug for ManifestSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManifestSigningKey")
            .field("pkcs8", &"[REDACTED]")
            .finish()
    }
}

/// Separately retainable authentication anchor for one exact manifest-chain head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedManifestHead {
    /// Manifest sequence authenticated by the signature.
    pub sequence: u64,
    /// Content identity of that exact canonical manifest body.
    pub manifest_id: ManifestId,
    /// Ed25519 public verification key.
    public_key: [u8; ED25519_PUBLIC_KEY_BYTES],
    /// Signature over the domain-separated sequence and manifest identity.
    signature: [u8; ED25519_SIGNATURE_BYTES],
}

impl TrustedManifestHead {
    /// Returns the Ed25519 public key bytes embedded in this external anchor.
    #[must_use]
    pub const fn public_key(&self) -> &[u8; ED25519_PUBLIC_KEY_BYTES] {
        &self.public_key
    }

    /// Returns the detached Ed25519 signature bytes.
    #[must_use]
    pub const fn signature(&self) -> &[u8; ED25519_SIGNATURE_BYTES] {
        &self.signature
    }

    /// Encodes this anchor as bounded canonical schema-v1 JSON suitable for storage
    /// outside the library directory.
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, TrustedHeadError> {
        let wire = TrustedManifestHeadWire {
            schema_version: TRUSTED_HEAD_SCHEMA_VERSION,
            algorithm: TRUSTED_HEAD_ALGORITHM.to_owned(),
            sequence: self.sequence,
            manifest_id: self.manifest_id.to_string(),
            public_key: encode_hex(&self.public_key),
            signature: encode_hex(&self.signature),
        };
        let bytes = serde_json::to_vec(&wire).map_err(TrustedHeadError::Json)?;
        if bytes.len() > MAX_TRUSTED_HEAD_BYTES {
            return Err(TrustedHeadError::AnchorTooLarge);
        }
        Ok(bytes)
    }

    /// Parses a canonical schema-v1 trusted-head JSON document.
    pub fn from_canonical_json(bytes: &[u8]) -> Result<Self, TrustedHeadError> {
        if bytes.len() > MAX_TRUSTED_HEAD_BYTES {
            return Err(TrustedHeadError::AnchorTooLarge);
        }
        let wire: TrustedManifestHeadWire =
            serde_json::from_slice(bytes).map_err(TrustedHeadError::Json)?;
        if wire.schema_version != TRUSTED_HEAD_SCHEMA_VERSION {
            return Err(TrustedHeadError::UnsupportedSchema(wire.schema_version));
        }
        if wire.algorithm != TRUSTED_HEAD_ALGORITHM {
            return Err(TrustedHeadError::UnsupportedAlgorithm(wire.algorithm));
        }
        if wire.sequence == 0 {
            return Err(TrustedHeadError::InvalidAnchor(
                "trusted manifest sequence must be positive",
            ));
        }
        let anchor = Self {
            sequence: wire.sequence,
            manifest_id: wire
                .manifest_id
                .parse()
                .map_err(|_| TrustedHeadError::InvalidAnchor("invalid manifest identity"))?,
            public_key: decode_hex_array(&wire.public_key)
                .map_err(|_| TrustedHeadError::InvalidAnchor("invalid Ed25519 public key"))?,
            signature: decode_hex_array(&wire.signature)
                .map_err(|_| TrustedHeadError::InvalidAnchor("invalid Ed25519 signature"))?,
        };
        let canonical = anchor.to_canonical_json()?;
        if canonical != bytes {
            return Err(TrustedHeadError::InvalidAnchor(
                "trusted head is not in canonical JSON form",
            ));
        }
        Ok(anchor)
    }

    fn signature_is_valid(&self) -> bool {
        signature::UnparsedPublicKey::new(&signature::ED25519, self.public_key)
            .verify(
                &trusted_head_message(self.sequence, self.manifest_id),
                &self.signature,
            )
            .is_ok()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustedManifestHeadWire {
    schema_version: u32,
    algorithm: String,
    sequence: u64,
    manifest_id: String,
    public_key: String,
    signature: String,
}

/// Signs the current validated manifest head for export to separately retained
/// storage. The returned anchor contains no secret key material.
pub fn sign_current_manifest_head(
    library: &Library,
    signing_key: &ManifestSigningKey,
) -> Result<TrustedManifestHead, TrustedHeadError> {
    let stored = validated_manifest_head(library)?;
    let sequence = stored.manifest.sequence;
    let key_pair = signing_key.key_pair()?;
    let signature = key_pair.sign(&trusted_head_message(sequence, stored.id));
    let public_key = key_pair.public_key().as_ref().try_into().map_err(|_| {
        TrustedHeadError::InvalidAnchor("Ed25519 public key has an unexpected length")
    })?;
    let signature = signature.as_ref().try_into().map_err(|_| {
        TrustedHeadError::InvalidAnchor("Ed25519 signature has an unexpected length")
    })?;
    Ok(TrustedManifestHead {
        sequence,
        manifest_id: stored.id,
        public_key,
        signature,
    })
}

fn validated_manifest_head(
    library: &Library,
) -> Result<wikisync_store::StoredManifest, TrustedHeadError> {
    let mut cursor = None;
    let mut expected_sequence = 1_u64;
    let mut predecessor = None;
    let mut run_ids = HashSet::new();
    let mut purge_ids = HashSet::new();
    let mut head = None;
    loop {
        let page = library.manifests_after(cursor, 1_000)?;
        if page.is_empty() {
            break;
        }
        for stored in page {
            if stored.manifest.sequence != expected_sequence {
                return Err(TrustedHeadError::InvalidManifestHistory(
                    "manifest append sequence has a gap",
                ));
            }
            if stored.manifest.predecessor != predecessor {
                return Err(TrustedHeadError::InvalidManifestHistory(
                    "manifest predecessor chain is broken",
                ));
            }
            if let Some(sync) = stored.manifest.sync()
                && !run_ids.insert(sync.run_id)
            {
                return Err(TrustedHeadError::InvalidManifestHistory(
                    "sync run occurs more than once in manifest chain",
                ));
            }
            if let Some(purge) = stored.manifest.purge() {
                let expected_pre_purge_head = predecessor.map(|id| (expected_sequence - 1, id));
                if purge.pre_purge_head_sequence.zip(purge.pre_purge_head_id)
                    != expected_pre_purge_head
                {
                    return Err(TrustedHeadError::InvalidManifestHistory(
                        "purge event pre-head does not authenticate its predecessor",
                    ));
                }
                if !purge_ids.insert(purge.purge_id) {
                    return Err(TrustedHeadError::InvalidManifestHistory(
                        "purge journal occurs more than once in manifest chain",
                    ));
                }
            }
            predecessor = Some(stored.id);
            cursor = Some(stored.manifest.sequence);
            expected_sequence = expected_sequence.checked_add(1).ok_or(
                TrustedHeadError::InvalidManifestHistory("manifest sequence overflowed"),
            )?;
            head = Some(stored);
        }
    }
    head.ok_or(TrustedHeadError::EmptyManifestHistory)
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
        manifest_revision_claims_examined: 0,
        manifest_page_head_claims_examined: 0,
        manifest_media_coverage: ManifestMediaCoverage::NotCovered,
        manifest_media_snapshots_examined: 0,
        purge_events_examined: 0,
        purges_pending_cleanup: 0,
        authorized_absences_verified: 0,
        metadata_records_at_start: 0,
        metadata_records_at_end: 0,
        metadata_records_examined: 0,
        trusted_head_authenticated: false,
        finding_count: 0,
        findings: Vec::new(),
        omitted_findings: 0,
    };
    let mut cursor = None;
    let mut missing_objects = Vec::new();

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
                        metadata_subject: None,
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
                                metadata_subject: None,
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
                Err(StoreError::ObjectNotFound(_)) => missing_objects.push(object_id),
                Err(error) => {
                    let is_absent = match &error {
                        StoreError::Io(error) => error.kind() == std::io::ErrorKind::NotFound,
                        _ => false,
                    };
                    push_finding(
                        &mut report,
                        options.max_retained_findings,
                        VerificationFinding {
                            kind: if is_absent {
                                VerificationFindingKind::UnexplainedObjectLoss
                            } else {
                                VerificationFindingKind::ObjectUnreadable
                            },
                            object_id: Some(object_id),
                            manifest_sequence: None,
                            metadata_subject: None,
                            message: error.to_string(),
                        },
                    );
                }
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
                metadata_subject: None,
                message,
            },
        );
    }
    if report.objects_examined != target {
        report.coverage = VerificationCoverage::Partial;
    }
    if options.scope == VerificationScope::Full {
        let authorized_absences =
            verify_manifest_history(library, options.max_retained_findings, &mut report)?;
        record_missing_objects(
            &mut report,
            options.max_retained_findings,
            &missing_objects,
            &authorized_absences,
        );
        verify_metadata_references(library, options, &mut report)?;
    } else {
        record_missing_objects(
            &mut report,
            options.max_retained_findings,
            &missing_objects,
            &HashSet::new(),
        );
    }

    Ok(report)
}

fn record_missing_objects(
    report: &mut VerificationReport,
    maximum_findings: usize,
    missing_objects: &[ObjectId],
    authorized_absences: &HashSet<ObjectId>,
) {
    for object_id in missing_objects {
        if authorized_absences.contains(object_id) {
            report.authorized_absences_verified =
                report.authorized_absences_verified.saturating_add(1);
        } else {
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::UnexplainedObjectLoss,
                    object_id: Some(*object_id),
                    manifest_sequence: None,
                    metadata_subject: None,
                    message: format!(
                        "object {object_id} has no verified readable representation and no exact authenticated authorized absence"
                    ),
                },
            );
        }
    }
}

/// Performs full local verification and authenticates the resulting manifest-chain
/// head against a separately supplied Ed25519 anchor.
///
/// A valid but older anchor is reported as a mismatch: callers should retain the
/// newest exported anchor they trust rather than silently accepting rollback or a
/// library that has advanced since export.
pub fn verify_library_against_trusted_head(
    library: &Library,
    options: VerificationOptions,
    trusted_head: &TrustedManifestHead,
) -> Result<VerificationReport, VerificationError> {
    if options.scope != VerificationScope::Full {
        return Err(VerificationError::TrustedHeadRequiresFullVerification);
    }
    let maximum_findings = options.max_retained_findings;
    let mut report = verify_library_with_options(library, options)?;
    if !trusted_head.signature_is_valid() {
        push_finding(
            &mut report,
            maximum_findings,
            VerificationFinding {
                kind: VerificationFindingKind::TrustedHeadSignatureInvalid,
                object_id: None,
                manifest_sequence: Some(trusted_head.sequence),
                metadata_subject: None,
                message: "external trusted-head Ed25519 signature is invalid".to_owned(),
            },
        );
        return Ok(report);
    }

    let local_head = (report.manifests_at_start > 0)
        .then_some(report.manifests_at_start)
        .and_then(|sequence| library.read_manifest(sequence).ok());
    let matches = local_head.as_ref().is_some_and(|stored| {
        stored.manifest.sequence == trusted_head.sequence && stored.id == trusted_head.manifest_id
    });
    if matches && report.manifests_at_start == report.manifests_at_end {
        report.trusted_head_authenticated = true;
    } else {
        let local_description = local_head.map_or_else(
            || "no readable local manifest head".to_owned(),
            |stored| {
                format!(
                    "local head sequence {} ({})",
                    stored.manifest.sequence, stored.id
                )
            },
        );
        push_finding(
            &mut report,
            maximum_findings,
            VerificationFinding {
                kind: VerificationFindingKind::TrustedHeadMismatch,
                object_id: None,
                manifest_sequence: Some(trusted_head.sequence),
                metadata_subject: None,
                message: format!(
                    "external trusted head sequence {} ({}) does not match {local_description}",
                    trusted_head.sequence, trusted_head.manifest_id
                ),
            },
        );
    }
    Ok(report)
}

fn trusted_head_message(sequence: u64, manifest_id: ManifestId) -> Vec<u8> {
    let mut message = Vec::with_capacity(TRUSTED_HEAD_DOMAIN.len() + 8 + 32);
    message.extend_from_slice(TRUSTED_HEAD_DOMAIN);
    message.extend_from_slice(&sequence.to_le_bytes());
    message.extend_from_slice(manifest_id.as_bytes());
    message
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_hex_array<const N: usize>(encoded: &str) -> Result<[u8; N], ()> {
    if encoded.len() != N * 2 || !encoded.is_ascii() {
        return Err(());
    }
    let mut decoded = [0_u8; N];
    for (index, byte) in decoded.iter_mut().enumerate() {
        let offset = index * 2;
        let high = decode_hex_nibble(encoded.as_bytes()[offset]).ok_or(())?;
        let low = decode_hex_nibble(encoded.as_bytes()[offset + 1]).ok_or(())?;
        *byte = (high << 4) | low;
    }
    Ok(decoded)
}

const fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn verify_metadata_references(
    library: &Library,
    options: VerificationOptions,
    report: &mut VerificationReport,
) -> Result<(), VerificationError> {
    let change_counter_at_start = library.integrity_metadata_change_counter()?;
    report.metadata_records_at_start = library.integrity_metadata_record_count()?;
    report.metadata_records_at_end = report.metadata_records_at_start;
    let mut cursor = None;
    while report.metadata_records_examined < report.metadata_records_at_start {
        let remaining = report.metadata_records_at_start - report.metadata_records_examined;
        let limit = u32::try_from(u64::from(options.page_size).min(remaining))
            .expect("metadata page limit is already bounded to u32");
        let page = library.integrity_metadata_records_after(cursor, limit)?;
        if page.is_empty() {
            report.coverage = VerificationCoverage::Partial;
            push_finding(
                report,
                options.max_retained_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::MetadataChangedDuringVerification,
                    object_id: None,
                    manifest_sequence: None,
                    metadata_subject: None,
                    message:
                        "metadata-reference enumeration ended before its starting record count"
                            .to_owned(),
                },
            );
            break;
        }
        for record in page {
            let subject = record.subject.clone();
            cursor = Some(record.cursor()?);
            report.metadata_records_examined =
                report.metadata_records_examined.checked_add(1).ok_or(
                    VerificationError::CounterOverflow("metadata records examined"),
                )?;
            let media_object_id = record.media_object.as_ref().map(|media| media.object_id);
            let media_reference_is_readable = !record.issues.iter().any(|issue| {
                matches!(
                    issue,
                    IntegrityMetadataIssue::MediaObjectMissing
                        | IntegrityMetadataIssue::MediaObjectWrongKind
                )
            });
            for issue in record.issues {
                let (kind, detail) = metadata_finding(issue);
                push_finding(
                    report,
                    options.max_retained_findings,
                    VerificationFinding {
                        kind,
                        object_id: media_object_id,
                        manifest_sequence: None,
                        metadata_subject: Some(subject.clone()),
                        message: format!("{subject:?}: {detail}"),
                    },
                );
            }
            if let Some(version) = record.search_transformer_version
                && version != PLAIN_TEXT_TRANSFORMER_VERSION.as_str()
            {
                push_finding(
                    report,
                    options.max_retained_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::SearchTransformerVersionMismatch,
                        object_id: None,
                        manifest_sequence: None,
                        metadata_subject: Some(subject.clone()),
                        message: format!(
                            "{subject:?}: search transformer version {version:?} is not current version {:?}",
                            PLAIN_TEXT_TRANSFORMER_VERSION.as_str()
                        ),
                    },
                );
            }
            if media_reference_is_readable && let Some(media_object) = record.media_object {
                match library.read_object(media_object.object_id) {
                    Ok(bytes) => {
                        let validation = validate_thumbnail(
                            &bytes,
                            &media_object.mime_type,
                            &integrity_thumbnail_limits(),
                        );
                        let metadata_matches = validation.is_ok_and(|validated| {
                            media_object.width == Some(validated.width)
                                && media_object.height == Some(validated.height)
                        });
                        if !metadata_matches {
                            push_finding(
                                report,
                                options.max_retained_findings,
                                VerificationFinding {
                                    kind: VerificationFindingKind::MediaMetadataMismatch,
                                    object_id: Some(media_object.object_id),
                                    manifest_sequence: None,
                                    metadata_subject: Some(subject.clone()),
                                    message: format!(
                                        "{subject:?}: canonical bytes fail complete bounded passive-raster validation or disagree with recorded dimensions"
                                    ),
                                },
                            );
                        }
                    }
                    Err(error) => push_finding(
                        report,
                        options.max_retained_findings,
                        VerificationFinding {
                            kind: VerificationFindingKind::MediaObjectUnreadable,
                            object_id: Some(media_object.object_id),
                            manifest_sequence: None,
                            metadata_subject: Some(subject.clone()),
                            message: format!(
                                "{subject:?}: media-linked canonical object failed verified read: {error}"
                            ),
                        },
                    ),
                }
            }
        }
    }

    report.metadata_records_at_end = library.integrity_metadata_record_count()?;
    let change_counter_at_end = library.integrity_metadata_change_counter()?;
    if report.metadata_records_at_end != report.metadata_records_at_start
        || change_counter_at_end != change_counter_at_start
    {
        report.coverage = VerificationCoverage::Partial;
        push_finding(
            report,
            options.max_retained_findings,
            VerificationFinding {
                kind: VerificationFindingKind::MetadataChangedDuringVerification,
                object_id: None,
                manifest_sequence: None,
                metadata_subject: None,
                message: format!(
                    "metadata changed during verification (record count {} to {}, SQLite change counter {} to {})",
                    report.metadata_records_at_start,
                    report.metadata_records_at_end,
                    change_counter_at_start,
                    change_counter_at_end
                ),
            },
        );
    }
    if report.metadata_records_examined != report.metadata_records_at_start {
        report.coverage = VerificationCoverage::Partial;
    }
    Ok(())
}

fn integrity_thumbnail_limits() -> ThumbnailLimits {
    let maximum_pixels =
        u64::from(MAX_THUMBNAIL_EDGE_PIXELS) * u64::from(MAX_THUMBNAIL_EDGE_PIXELS);
    ThumbnailLimits {
        max_encoded_bytes: MAX_THUMBNAIL_BYTES,
        max_width: MAX_THUMBNAIL_EDGE_PIXELS,
        max_height: MAX_THUMBNAIL_EDGE_PIXELS,
        max_pixels: maximum_pixels,
        max_decoded_bytes: maximum_pixels * MAX_THUMBNAIL_BYTES_PER_PIXEL,
    }
}
const fn metadata_finding(
    issue: IntegrityMetadataIssue,
) -> (VerificationFindingKind, &'static str) {
    match issue {
        IntegrityMetadataIssue::RevisionPageMissing => (
            VerificationFindingKind::RevisionPageUnreachable,
            "revision's owning page is absent",
        ),
        IntegrityMetadataIssue::RevisionObjectMissing => (
            VerificationFindingKind::RevisionObjectUnreachable,
            "revision's canonical content object is absent",
        ),
        IntegrityMetadataIssue::RevisionParentWrongPage => (
            VerificationFindingKind::RevisionParentPageMismatch,
            "captured parent revision belongs to another page",
        ),
        IntegrityMetadataIssue::RevisionParentSelfReference => (
            VerificationFindingKind::RevisionParentSelfReference,
            "revision identifies itself as its parent",
        ),
        IntegrityMetadataIssue::PageHeadRevisionMissing => (
            VerificationFindingKind::PageHeadRevisionUnreachable,
            "page head revision is absent",
        ),
        IntegrityMetadataIssue::PageHeadRevisionWrongPage => (
            VerificationFindingKind::PageHeadRevisionPageMismatch,
            "page head revision belongs to another page",
        ),
        IntegrityMetadataIssue::CheckpointCollectionWikiMismatch => (
            VerificationFindingKind::CheckpointCollectionMismatch,
            "checkpoint collection is absent or belongs to another wiki",
        ),
        IntegrityMetadataIssue::CheckpointRunMissing => (
            VerificationFindingKind::CheckpointRunUnreachable,
            "committed checkpoint has no reachable advancing run",
        ),
        IntegrityMetadataIssue::CheckpointRunNotSucceeded => (
            VerificationFindingKind::CheckpointRunNotSucceeded,
            "checkpoint's advancing run has not succeeded",
        ),
        IntegrityMetadataIssue::CheckpointRunScopeMismatch => (
            VerificationFindingKind::CheckpointRunScopeMismatch,
            "checkpoint and advancing run have different scope",
        ),
        IntegrityMetadataIssue::CheckpointBoundaryMismatch => (
            VerificationFindingKind::CheckpointBoundaryMismatch,
            "checkpoint boundary differs from advancing run candidate",
        ),
        IntegrityMetadataIssue::SearchPageMissing => (
            VerificationFindingKind::SearchPageUnreachable,
            "search document page is absent",
        ),
        IntegrityMetadataIssue::SearchRevisionMissing => (
            VerificationFindingKind::SearchRevisionUnreachable,
            "search document revision is absent",
        ),
        IntegrityMetadataIssue::SearchRevisionWrongPage => (
            VerificationFindingKind::SearchRevisionPageMismatch,
            "search document revision belongs to another page",
        ),
        IntegrityMetadataIssue::SearchRevisionNotCurrent => (
            VerificationFindingKind::SearchRevisionNotCurrent,
            "search document does not point to the page's current revision",
        ),
        IntegrityMetadataIssue::SearchFtsRowMissing => (
            VerificationFindingKind::SearchFtsRowMissing,
            "search document has no FTS row",
        ),
        IntegrityMetadataIssue::SearchFtsRowOrphan => (
            VerificationFindingKind::SearchFtsRowOrphan,
            "FTS row has no search document",
        ),
        IntegrityMetadataIssue::MediaObjectMissing => (
            VerificationFindingKind::MediaObjectUnreachable,
            "media metadata's canonical content object is absent",
        ),
        IntegrityMetadataIssue::MediaObjectWrongKind => (
            VerificationFindingKind::MediaObjectKindMismatch,
            "media metadata points to a non-media logical object",
        ),
        IntegrityMetadataIssue::MediaMetadataInvalid => (
            VerificationFindingKind::MediaMetadataMismatch,
            "media metadata violates bounded canonical-object or thumbnail invariants",
        ),
        IntegrityMetadataIssue::PageMediaRevisionMissing => (
            VerificationFindingKind::MediaPlacementRevisionUnreachable,
            "media placement's revision is absent from its recorded wiki",
        ),
        IntegrityMetadataIssue::PageMediaPageMissing => (
            VerificationFindingKind::MediaPlacementPageUnreachable,
            "media placement's revision has no owning page in its recorded wiki",
        ),
        IntegrityMetadataIssue::PageMediaMediaMissing => (
            VerificationFindingKind::MediaPlacementMediaUnreachable,
            "media placement's immutable media version is absent from its recorded wiki",
        ),
        IntegrityMetadataIssue::PageMediaMetadataInvalid => (
            VerificationFindingKind::MediaPlacementMetadataMismatch,
            "media placement violates bounded ordering or display-metadata invariants",
        ),
    }
}

/// Failure to create, encode, parse, or sign an external trusted manifest head.
#[derive(Debug)]
pub enum TrustedHeadError {
    /// Library metadata or the manifest file could not be read.
    Store(StoreError),
    /// The operating system random source could not generate a key.
    KeyGeneration,
    /// Imported PKCS#8 bytes were not a supported Ed25519 key document.
    InvalidSigningKey,
    /// No manifest exists to authenticate.
    EmptyManifestHistory,
    /// The local manifest inventory was readable but not one strict append chain.
    InvalidManifestHistory(&'static str),
    /// Trusted-head JSON encoding or decoding failed.
    Json(serde_json::Error),
    /// The trusted-head document exceeded its fixed input bound.
    AnchorTooLarge,
    /// The trusted-head schema version is not supported.
    UnsupportedSchema(u32),
    /// The trusted-head signature algorithm is not supported.
    UnsupportedAlgorithm(String),
    /// A trusted-head field or its canonical encoding was invalid.
    InvalidAnchor(&'static str),
}

impl fmt::Display for TrustedHeadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "trusted-head store read failed: {error}"),
            Self::KeyGeneration => formatter.write_str("Ed25519 key generation failed"),
            Self::InvalidSigningKey => formatter.write_str("invalid Ed25519 PKCS#8 signing key"),
            Self::EmptyManifestHistory => {
                formatter.write_str("cannot sign an empty manifest history")
            }
            Self::InvalidManifestHistory(message) => {
                write!(formatter, "cannot sign invalid manifest history: {message}")
            }
            Self::Json(error) => write!(formatter, "trusted-head JSON failed: {error}"),
            Self::AnchorTooLarge => write!(
                formatter,
                "trusted-head document exceeds the {MAX_TRUSTED_HEAD_BYTES}-byte bound"
            ),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "unsupported trusted-head schema version {version}"
                )
            }
            Self::UnsupportedAlgorithm(algorithm) => {
                write!(
                    formatter,
                    "unsupported trusted-head algorithm {algorithm:?}"
                )
            }
            Self::InvalidAnchor(message) => write!(formatter, "invalid trusted head: {message}"),
        }
    }
}

impl Error for TrustedHeadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<StoreError> for TrustedHeadError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn verify_manifest_history(
    library: &Library,
    maximum_findings: usize,
    report: &mut VerificationReport,
) -> Result<HashSet<ObjectId>, VerificationError> {
    let mut authorized_absences = HashSet::new();
    let claim_change_counter_at_start = library.integrity_metadata_change_counter()?;
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
                    metadata_subject: None,
                    message: error.to_string(),
                },
            );
            return Ok(authorized_absences);
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
                    metadata_subject: None,
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
                    metadata_subject: None,
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
    let mut latest_by_scope: HashMap<(u64, Option<u64>), StoredManifest> = HashMap::new();
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
                    metadata_subject: None,
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
                        metadata_subject: None,
                        message: error.to_string(),
                    },
                );
                previous = None;
                continue;
            }
        };
        report.manifests_identity_verified = report.manifests_identity_verified.saturating_add(1);
        verify_manifest_metadata_claims(library, &stored, maximum_findings, report)?;

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
                    metadata_subject: None,
                    message: format!(
                        "manifest predecessor {:?} does not match prior verified identity {:?}",
                        stored.manifest.predecessor, expected_predecessor
                    ),
                },
            );
        }
        verify_purge_event(
            library,
            &stored,
            expected_predecessor.map(|id| (sequence - 1, id)),
            maximum_findings,
            report,
            &mut authorized_absences,
        );
        previous = Some((sequence, stored.id));

        if let Some(sync) = stored.manifest.sync() {
            if !represented_runs.insert(sync.run_id) {
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::DuplicateManifestRun,
                        object_id: None,
                        manifest_sequence: Some(sequence),
                        metadata_subject: None,
                        message: format!(
                            "sync run {} is represented by more than one manifest",
                            sync.run_id
                        ),
                    },
                );
            }
            let status = library.sync_run_status(sync.run_id)?;
            if status.is_none_or(|status| status.state != SyncRunState::Succeeded) {
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::ManifestRunNotSucceeded,
                        object_id: None,
                        manifest_sequence: Some(sequence),
                        metadata_subject: None,
                        message: format!(
                            "manifest refers to absent or unsuccessful sync run {}",
                            sync.run_id
                        ),
                    },
                );
            }
            latest_by_scope.insert(
                (sync.wiki_id.get(), sync.collection_id.map(|id| id.get())),
                stored,
            );
        }
    }

    record_manifest_claim_scan_stability(
        library,
        claim_change_counter_at_start,
        maximum_findings,
        report,
    )?;

    verify_manifest_media_snapshots(library, latest_by_scope, maximum_findings, report)?;

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
                        metadata_subject: None,
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
                        metadata_subject: None,
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
                    metadata_subject: None,
                    message: error.to_string(),
                },
            );
        }
    }
    Ok(authorized_absences)
}

fn verify_purge_event(
    library: &Library,
    stored: &StoredManifest,
    expected_pre_purge_head: Option<(u64, ManifestId)>,
    maximum_findings: usize,
    report: &mut VerificationReport,
    authorized_absences: &mut HashSet<ObjectId>,
) {
    let Some(event) = stored.manifest.purge() else {
        return;
    };
    report.purge_events_examined = report.purge_events_examined.saturating_add(1);
    let event_pre_purge_head = event.pre_purge_head_sequence.zip(event.pre_purge_head_id);
    if event_pre_purge_head != expected_pre_purge_head {
        push_finding(
            report,
            maximum_findings,
            VerificationFinding {
                kind: VerificationFindingKind::PurgeJournalMismatch,
                object_id: None,
                manifest_sequence: Some(stored.manifest.sequence),
                metadata_subject: None,
                message: format!(
                    "purge event {} commits pre-purge head {:?}, but its authenticated chain position requires {:?}",
                    event.purge_id, event_pre_purge_head, expected_pre_purge_head
                ),
            },
        );
    }

    match library.purge_verification_snapshot(event.purge_id) {
        Ok(snapshot) => {
            if snapshot.expected_manifest != *event {
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::PurgeJournalMismatch,
                        object_id: None,
                        manifest_sequence: Some(stored.manifest.sequence),
                        metadata_subject: None,
                        message: format!(
                            "purge event {} does not exactly match its durable journal binding",
                            event.purge_id
                        ),
                    },
                );
            } else if snapshot.shared_object_count != 0 {
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::PurgeSharedReferenceViolation,
                        object_id: None,
                        manifest_sequence: Some(stored.manifest.sequence),
                        metadata_subject: None,
                        message: format!(
                            "purge journal {} contains {} object(s) still required by retained references",
                            event.purge_id, snapshot.shared_object_count
                        ),
                    },
                );
            } else {
                let cleanup_valid = match library.verify_purge_cleanup_state(event.purge_id) {
                    Ok(progress) if progress.state == snapshot.state => true,
                    Ok(_) => {
                        push_cleanup_mismatch(
                            report,
                            maximum_findings,
                            stored.manifest.sequence,
                            event.purge_id,
                            "cleanup progress disagrees with its durable journal state",
                        );
                        false
                    }
                    Err(error) => {
                        push_cleanup_mismatch(
                            report,
                            maximum_findings,
                            stored.manifest.sequence,
                            event.purge_id,
                            &format!("cleanup state failed verification: {error}"),
                        );
                        false
                    }
                };
                if cleanup_valid {
                    match snapshot.state {
                        PurgeJournalState::Authorized | PurgeJournalState::Repacking => {
                            report.purges_pending_cleanup =
                                report.purges_pending_cleanup.saturating_add(1);
                        }
                        PurgeJournalState::Cleaning => {
                            report.purges_pending_cleanup =
                                report.purges_pending_cleanup.saturating_add(1);
                            collect_authorized_absences(
                                library,
                                stored.manifest.sequence,
                                event,
                                snapshot.state,
                                maximum_findings,
                                report,
                                authorized_absences,
                            );
                        }
                        PurgeJournalState::Succeeded => collect_authorized_absences(
                            library,
                            stored.manifest.sequence,
                            event,
                            snapshot.state,
                            maximum_findings,
                            report,
                            authorized_absences,
                        ),
                        PurgeJournalState::Failed => push_finding(
                            report,
                            maximum_findings,
                            VerificationFinding {
                                kind: VerificationFindingKind::PurgeJournalMismatch,
                                object_id: None,
                                manifest_sequence: Some(stored.manifest.sequence),
                                metadata_subject: None,
                                message: format!(
                                    "purge journal {} is durably marked failed",
                                    event.purge_id
                                ),
                            },
                        ),
                    }
                }
            }
        }
        Err(StoreError::PurgeNotFound(_)) => push_finding(
            report,
            maximum_findings,
            VerificationFinding {
                kind: VerificationFindingKind::PurgeJournalMissing,
                object_id: None,
                manifest_sequence: Some(stored.manifest.sequence),
                metadata_subject: None,
                message: format!(
                    "authenticated purge event {} has no durable cleanup journal",
                    event.purge_id
                ),
            },
        ),
        Err(StoreError::StalePurgePreview(_)) => push_finding(
            report,
            maximum_findings,
            VerificationFinding {
                kind: VerificationFindingKind::PurgeJournalMismatch,
                object_id: None,
                manifest_sequence: Some(stored.manifest.sequence),
                metadata_subject: None,
                message: format!(
                    "purge journal {} no longer matches its retained collection tombstone",
                    event.purge_id
                ),
            },
        ),
        Err(error) => push_finding(
            report,
            maximum_findings,
            VerificationFinding {
                kind: VerificationFindingKind::PurgeInventoryMismatch,
                object_id: None,
                manifest_sequence: Some(stored.manifest.sequence),
                metadata_subject: None,
                message: format!(
                    "purge journal {} could not reproduce its authenticated inventory binding: {error}",
                    event.purge_id
                ),
            },
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_authorized_absences(
    library: &Library,
    manifest_sequence: u64,
    event: &wikisync_store::PurgeManifest,
    state: PurgeJournalState,
    maximum_findings: usize,
    report: &mut VerificationReport,
    authorized_absences: &mut HashSet<ObjectId>,
) {
    match library.verify_purge_cleanup_state(event.purge_id) {
        Ok(progress)
            if progress.state == state
                && progress.manifest_installed
                && (state != PurgeJournalState::Succeeded
                    || (progress.pending_pack_count == 0
                        && progress.replacement_ready_pack_count == 0
                        && progress.pending_file_count == 0
                        && progress.unlinking_file_count == 0)) => {}
        Ok(_) => {
            push_cleanup_mismatch(
                report,
                maximum_findings,
                manifest_sequence,
                event.purge_id,
                "cleanup progress is incompatible with its authenticated journal phase",
            );
            return;
        }
        Err(error) => {
            push_cleanup_mismatch(
                report,
                maximum_findings,
                manifest_sequence,
                event.purge_id,
                &format!("cleanup progress is unreadable: {error}"),
            );
            return;
        }
    }

    let mut cursor = None;
    let mut examined = 0_u64;
    let mut exact_absences = Vec::new();
    let mut valid = true;
    loop {
        let objects = match library.purge_objects_after(event.purge_id, cursor, 1_000) {
            Ok(objects) => objects,
            Err(error) => {
                push_cleanup_mismatch(
                    report,
                    maximum_findings,
                    manifest_sequence,
                    event.purge_id,
                    &format!("authorized-absence inventory is unreadable: {error}"),
                );
                return;
            }
        };
        if objects.is_empty() {
            break;
        }
        for selected in &objects {
            examined = examined.saturating_add(1);
            cursor = Some(selected.object.id);
            let absence = match library
                .purge_authorized_absence_for_purge(event.purge_id, selected.object.id)
            {
                Ok(Some(absence)) if absence.object == selected.object => absence,
                Ok(_) => {
                    valid = false;
                    push_cleanup_mismatch(
                        report,
                        maximum_findings,
                        manifest_sequence,
                        event.purge_id,
                        &format!(
                            "journal object {} lacks its exact positive authorized-absence record",
                            selected.object.id
                        ),
                    );
                    continue;
                }
                Err(error) => {
                    valid = false;
                    push_cleanup_mismatch(
                        report,
                        maximum_findings,
                        manifest_sequence,
                        event.purge_id,
                        &format!(
                            "authorized-absence record for {} is unreadable: {error}",
                            selected.object.id
                        ),
                    );
                    continue;
                }
            };
            match (
                absence.superseded_at,
                library.read_object(selected.object.id),
            ) {
                (None, Err(StoreError::ObjectNotFound(_))) => {
                    exact_absences.push(selected.object.id);
                }
                (None, Ok(_)) => {
                    valid = false;
                    push_cleanup_mismatch(
                        report,
                        maximum_findings,
                        manifest_sequence,
                        event.purge_id,
                        &format!(
                            "authorized-absence object {} still has a verified readable location",
                            selected.object.id
                        ),
                    );
                }
                (None, Err(error)) => {
                    valid = false;
                    push_cleanup_mismatch(
                        report,
                        maximum_findings,
                        manifest_sequence,
                        event.purge_id,
                        &format!(
                            "authorized-absence object {} failed with an unexplained storage error: {error}",
                            selected.object.id
                        ),
                    );
                }
                (Some(_), Ok(_)) if state == PurgeJournalState::Succeeded => {}
                (Some(_), Ok(_)) => {
                    valid = false;
                    push_cleanup_mismatch(
                        report,
                        maximum_findings,
                        manifest_sequence,
                        event.purge_id,
                        &format!(
                            "absence for object {} was superseded before purge completion",
                            selected.object.id
                        ),
                    );
                }
                (Some(_), Err(error)) => {
                    valid = false;
                    push_cleanup_mismatch(
                        report,
                        maximum_findings,
                        manifest_sequence,
                        event.purge_id,
                        &format!(
                            "superseded absence object {} is not normally readable: {error}",
                            selected.object.id
                        ),
                    );
                }
            }
        }
        if objects.len() < 1_000 {
            break;
        }
    }
    if examined != event.object_count {
        valid = false;
        push_cleanup_mismatch(
            report,
            maximum_findings,
            manifest_sequence,
            event.purge_id,
            &format!(
                "authorized-absence inventory contains {examined} objects, but the event commits {}",
                event.object_count
            ),
        );
    }
    if valid {
        authorized_absences.extend(exact_absences);
    }
}

fn push_cleanup_mismatch(
    report: &mut VerificationReport,
    maximum_findings: usize,
    manifest_sequence: u64,
    purge_id: u64,
    detail: &str,
) {
    push_finding(
        report,
        maximum_findings,
        VerificationFinding {
            kind: VerificationFindingKind::PurgeCleanupMismatch,
            object_id: None,
            manifest_sequence: Some(manifest_sequence),
            metadata_subject: None,
            message: format!("purge cleanup {purge_id} is inconsistent: {detail}"),
        },
    );
}

fn record_manifest_claim_scan_stability(
    library: &Library,
    change_counter_at_start: u64,
    maximum_findings: usize,
    report: &mut VerificationReport,
) -> Result<(), VerificationError> {
    let change_counter_at_end = library.integrity_metadata_change_counter()?;
    if change_counter_at_end != change_counter_at_start {
        report.coverage = VerificationCoverage::Partial;
        push_finding(
            report,
            maximum_findings,
            VerificationFinding {
                kind: VerificationFindingKind::ManifestClaimsChangedDuringVerification,
                object_id: None,
                manifest_sequence: None,
                metadata_subject: None,
                message: format!(
                    "retained metadata changed during manifest claim replay (SQLite change counter {change_counter_at_start} to {change_counter_at_end})"
                ),
            },
        );
    }
    Ok(())
}

fn verify_manifest_metadata_claims(
    library: &Library,
    stored: &StoredManifest,
    maximum_findings: usize,
    report: &mut VerificationReport,
) -> Result<(), VerificationError> {
    let entry = &stored.manifest;
    let Some(manifest) = entry.sync() else {
        return Ok(());
    };
    let mut verify_revision_claim = |claim: &ManifestRevision| -> Result<(), VerificationError> {
        report.manifest_revision_claims_examined = report
            .manifest_revision_claims_examined
            .checked_add(1)
            .ok_or(VerificationError::CounterOverflow(
                "manifest revision claims examined",
            ))?;
        let revision = library.revision(manifest.wiki_id, claim.revision_id)?;
        match revision {
            None => push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestRevisionClaimMissing,
                    object_id: Some(claim.content_object_id),
                    manifest_sequence: Some(entry.sequence),
                    metadata_subject: None,
                    message: format!(
                        "manifested revision {} for page {} is absent from retained metadata",
                        claim.revision_id.get(),
                        claim.page_id.get()
                    ),
                },
            ),
            Some(revision) => {
                if revision.page_id != claim.page_id {
                    push_finding(
                        report,
                        maximum_findings,
                        VerificationFinding {
                            kind: VerificationFindingKind::ManifestRevisionClaimPageMismatch,
                            object_id: Some(claim.content_object_id),
                            manifest_sequence: Some(entry.sequence),
                            metadata_subject: None,
                            message: format!(
                                "manifested revision {} belonged to page {}, but retained metadata assigns it to page {}",
                                claim.revision_id.get(),
                                claim.page_id.get(),
                                revision.page_id.get()
                            ),
                        },
                    );
                }
                if revision.content_object_id != claim.content_object_id {
                    push_finding(
                        report,
                        maximum_findings,
                        VerificationFinding {
                            kind: VerificationFindingKind::ManifestRevisionClaimObjectMismatch,
                            object_id: Some(revision.content_object_id),
                            manifest_sequence: Some(entry.sequence),
                            metadata_subject: None,
                            message: format!(
                                "manifested revision {} selected object {}, but retained metadata selects {}",
                                claim.revision_id.get(),
                                claim.content_object_id,
                                revision.content_object_id
                            ),
                        },
                    );
                }
            }
        }
        if library.page(manifest.wiki_id, claim.page_id)?.is_none() {
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestRevisionClaimPageMissing,
                    object_id: Some(claim.content_object_id),
                    manifest_sequence: Some(entry.sequence),
                    metadata_subject: None,
                    message: format!(
                        "manifested revision {} refers to retained page {} that is absent",
                        claim.revision_id.get(),
                        claim.page_id.get()
                    ),
                },
            );
        }
        Ok(())
    };
    for claim in &manifest.introduced_revisions {
        verify_revision_claim(claim)?;
    }
    for descriptor in stored
        .shards
        .iter()
        .filter(|descriptor| descriptor.kind == ManifestShardKind::IntroducedRevisions)
    {
        let ManifestShard::IntroducedRevisions(claims) = library.read_manifest_shard(descriptor)?
        else {
            unreachable!("descriptor kind verified by shard reader")
        };
        for claim in &claims {
            verify_revision_claim(claim)?;
        }
    }

    let mut verify_page_head = |head: &ManifestPageHead| -> Result<(), VerificationError> {
        let Some(revision_id) = head.revision_id else {
            return Ok(());
        };
        report.manifest_page_head_claims_examined = report
            .manifest_page_head_claims_examined
            .checked_add(1)
            .ok_or(VerificationError::CounterOverflow(
                "manifest page-head claims examined",
            ))?;
        if library.page(manifest.wiki_id, head.page_id)?.is_none() {
            push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestPageHeadClaimPageMissing,
                    object_id: None,
                    manifest_sequence: Some(entry.sequence),
                    metadata_subject: None,
                    message: format!(
                        "manifested positive head for page {} has no retained page metadata",
                        head.page_id.get()
                    ),
                },
            );
        }
        match library.revision(manifest.wiki_id, revision_id)? {
            None => push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestPageHeadClaimRevisionMissing,
                    object_id: None,
                    manifest_sequence: Some(entry.sequence),
                    metadata_subject: None,
                    message: format!(
                        "manifested head revision {} for page {} is absent from retained metadata",
                        revision_id.get(),
                        head.page_id.get()
                    ),
                },
            ),
            Some(revision) if revision.page_id != head.page_id => push_finding(
                report,
                maximum_findings,
                VerificationFinding {
                    kind: VerificationFindingKind::ManifestPageHeadClaimRevisionPageMismatch,
                    object_id: Some(revision.content_object_id),
                    manifest_sequence: Some(entry.sequence),
                    metadata_subject: None,
                    message: format!(
                        "manifested head revision {} belonged to page {}, but retained metadata assigns it to page {}",
                        revision_id.get(),
                        head.page_id.get(),
                        revision.page_id.get()
                    ),
                },
            ),
            Some(_) => {}
        }
        Ok(())
    };
    for head in &manifest.page_heads {
        verify_page_head(head)?;
    }
    for descriptor in stored
        .shards
        .iter()
        .filter(|descriptor| descriptor.kind == ManifestShardKind::PageHeads)
    {
        let ManifestShard::PageHeads(heads) = library.read_manifest_shard(descriptor)? else {
            unreachable!("descriptor kind verified by shard reader")
        };
        for head in &heads {
            verify_page_head(head)?;
        }
    }
    Ok(())
}

fn verify_manifest_media_snapshots(
    library: &Library,
    latest_by_scope: HashMap<(u64, Option<u64>), StoredManifest>,
    maximum_findings: usize,
    report: &mut VerificationReport,
) -> Result<(), VerificationError> {
    let scope_count = latest_by_scope.len();
    let covered_count = latest_by_scope
        .values()
        .filter(|stored| {
            stored
                .manifest
                .sync()
                .is_some_and(|sync| sync.media_snapshot.is_some())
        })
        .count();
    report.manifest_media_coverage = match (covered_count, scope_count) {
        (0, _) => ManifestMediaCoverage::NotCovered,
        (covered, total) if covered == total => ManifestMediaCoverage::Complete,
        _ => ManifestMediaCoverage::Partial,
    };
    if covered_count == 0 {
        return Ok(());
    }

    let change_counter_at_start = library.integrity_metadata_change_counter()?;
    let mut manifests = latest_by_scope.into_values().collect::<Vec<_>>();
    manifests.sort_by_key(|stored| stored.manifest.sequence);
    for stored in manifests {
        let sync = stored
            .manifest
            .sync()
            .expect("latest scope map contains only synchronization events");
        let Some(expected) = sync.media_snapshot.as_ref() else {
            continue;
        };
        report.manifest_media_snapshots_examined =
            report.manifest_media_snapshots_examined.saturating_add(1);
        let current = library.manifest_media_snapshot(sync.wiki_id, sync.collection_id)?;
        let current_inventory = current
            .inventory
            .iter()
            .map(|media| {
                (
                    (
                        media.media_id.get(),
                        media.source_sha1.as_str(),
                        media.content_object_id,
                    ),
                    media,
                )
            })
            .collect::<HashMap<_, _>>();
        let expected_inventory = expected
            .inventory
            .iter()
            .map(|media| {
                (
                    (
                        media.media_id.get(),
                        media.source_sha1.as_str(),
                        media.content_object_id,
                    ),
                    media,
                )
            })
            .collect::<HashMap<_, _>>();
        for media in &expected.inventory {
            let key = (
                media.media_id.get(),
                media.source_sha1.as_str(),
                media.content_object_id,
            );
            match current_inventory.get(&key) {
                None => push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::ManifestMediaDeleted,
                        object_id: Some(media.content_object_id),
                        manifest_sequence: Some(stored.manifest.sequence),
                        metadata_subject: None,
                        message: format!(
                            "manifested media {} version {:?} is absent from the current scope inventory",
                            media.media_id.get(),
                            media.source_sha1
                        ),
                    },
                ),
                Some(current) if current.metadata_identity != media.metadata_identity => {
                    push_finding(
                        report,
                        maximum_findings,
                        VerificationFinding {
                            kind: VerificationFindingKind::ManifestMediaTampered,
                            object_id: Some(media.content_object_id),
                            manifest_sequence: Some(stored.manifest.sequence),
                            metadata_subject: None,
                            message: format!(
                                "media {} metadata identity changed from {} to {}",
                                media.media_id.get(),
                                media.metadata_identity,
                                current.metadata_identity
                            ),
                        },
                    );
                }
                Some(_) => {}
            }
        }
        for media in &current.inventory {
            let key = (
                media.media_id.get(),
                media.source_sha1.as_str(),
                media.content_object_id,
            );
            if !expected_inventory.contains_key(&key) {
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::ManifestMediaInventoryChanged,
                        object_id: Some(media.content_object_id),
                        manifest_sequence: Some(stored.manifest.sequence),
                        metadata_subject: None,
                        message: format!(
                            "current scope contains media {} version {:?} absent from its latest manifest",
                            media.media_id.get(),
                            media.source_sha1
                        ),
                    },
                );
            }
        }

        let current_placements = current
            .placements
            .iter()
            .map(|placement| {
                (
                    (placement.revision_id.get(), placement.placement_index),
                    placement,
                )
            })
            .collect::<HashMap<_, _>>();
        let expected_placements = expected
            .placements
            .iter()
            .map(|placement| {
                (
                    (placement.revision_id.get(), placement.placement_index),
                    placement,
                )
            })
            .collect::<HashMap<_, _>>();
        for placement in &expected.placements {
            let key = (placement.revision_id.get(), placement.placement_index);
            match current_placements.get(&key) {
                None => push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::ManifestMediaPlacementDeleted,
                        object_id: Some(placement.content_object_id),
                        manifest_sequence: Some(stored.manifest.sequence),
                        metadata_subject: None,
                        message: format!(
                            "manifested media placement revision {} index {} is absent",
                            placement.revision_id.get(),
                            placement.placement_index
                        ),
                    },
                ),
                Some(current)
                    if current.media_id != placement.media_id
                        || current.source_sha1 != placement.source_sha1
                        || current.content_object_id != placement.content_object_id =>
                {
                    push_finding(
                        report,
                        maximum_findings,
                        VerificationFinding {
                            kind: VerificationFindingKind::ManifestMediaPlacementSwapped,
                            object_id: Some(current.content_object_id),
                            manifest_sequence: Some(stored.manifest.sequence),
                            metadata_subject: None,
                            message: format!(
                                "media placement revision {} index {} selects a different rendition",
                                placement.revision_id.get(),
                                placement.placement_index
                            ),
                        },
                    );
                }
                Some(current) if current.placement_identity != placement.placement_identity => {
                    push_finding(
                        report,
                        maximum_findings,
                        VerificationFinding {
                            kind: VerificationFindingKind::ManifestMediaPlacementTampered,
                            object_id: Some(placement.content_object_id),
                            manifest_sequence: Some(stored.manifest.sequence),
                            metadata_subject: None,
                            message: format!(
                                "media placement revision {} index {} display metadata changed",
                                placement.revision_id.get(),
                                placement.placement_index
                            ),
                        },
                    );
                }
                Some(_) => {}
            }
        }
        for placement in &current.placements {
            let key = (placement.revision_id.get(), placement.placement_index);
            if !expected_placements.contains_key(&key) {
                push_finding(
                    report,
                    maximum_findings,
                    VerificationFinding {
                        kind: VerificationFindingKind::ManifestMediaInventoryChanged,
                        object_id: Some(placement.content_object_id),
                        manifest_sequence: Some(stored.manifest.sequence),
                        metadata_subject: None,
                        message: format!(
                            "current scope contains media placement revision {} index {} absent from its latest manifest",
                            placement.revision_id.get(),
                            placement.placement_index
                        ),
                    },
                );
            }
        }
    }

    let change_counter_at_end = library.integrity_metadata_change_counter()?;
    if change_counter_at_end != change_counter_at_start {
        report.coverage = VerificationCoverage::Partial;
        push_finding(
            report,
            maximum_findings,
            VerificationFinding {
                kind: VerificationFindingKind::ManifestMediaInventoryChangedDuringVerification,
                object_id: None,
                manifest_sequence: None,
                metadata_subject: None,
                message: "SQLite media inventory changed during manifest comparison".to_owned(),
            },
        );
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
    /// An external trust anchor is only meaningful with a full manifest scan.
    TrustedHeadRequiresFullVerification,
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
            Self::TrustedHeadRequiresFullVerification => {
                formatter.write_str("trusted-head authentication requires full verification scope")
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

    use rusqlite::params;
    use tempfile::TempDir;
    use wikisync_core::{CollectionId, MediaId, PageId, PageTitle, RevisionId, ThumbnailPolicy};
    use wikisync_store::{
        CurrentRevisionCapture, Library, MediaPlacementKind, ObjectKind, PurgeAuthorization,
        RevisionMediaPlacement, StoreConfig, SyncRunKind, ThumbnailCapture, ThumbnailMimeType,
    };

    use super::*;

    const VALID_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

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

    fn manifested_revision_history() -> (TempDir, Library) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "manifest claims")
            .expect("collection");
        let page_id = PageId::new(10).expect("page");
        let title = PageTitle::new("Manifest claim page").expect("title");
        for (revision, parent, timestamp, source, candidate, kind) in [
            (
                20,
                None,
                "2026-08-21T00:00:00Z",
                b"manifested historical source".as_slice(),
                100,
                SyncRunKind::Bootstrap,
            ),
            (
                21,
                Some(20),
                "2026-08-22T00:00:00Z",
                b"manifested current source".as_slice(),
                101,
                SyncRunKind::Update,
            ),
        ] {
            library
                .capture_current_revision(
                    wiki_id,
                    collection_id,
                    &CurrentRevisionCapture {
                        page_id,
                        namespace: 0,
                        title: &title,
                        revision_id: RevisionId::new(revision).expect("revision"),
                        parent_id: parent.map(|id| RevisionId::new(id).expect("parent")),
                        timestamp,
                        author: Some("Fixture author"),
                        author_id: Some(1),
                        comment: Some("manifest claim fixture"),
                        minor: false,
                        upstream_sha1: None,
                        content_model: "wikitext",
                        source,
                    },
                )
                .expect("capture revision");
            let run_id = library
                .start_or_resume_sync_run(wiki_id, Some(collection_id), kind, candidate)
                .expect("start run")
                .status
                .run_id;
            library
                .complete_sync_run(run_id, Some("manifest-claim-cursor"))
                .expect("complete run");
            library.append_sync_manifest(run_id).expect("manifest run");
        }
        (directory, library)
    }

    fn authenticated_purge_fixture() -> (TempDir, Library, u64, ObjectId) {
        authenticated_purge_fixture_with_storage(false)
    }

    fn authenticated_purge_fixture_with_storage(
        pack_object: bool,
    ) -> (TempDir, Library, u64, ObjectId) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Purge verification fixture")
            .expect("collection");
        let title = PageTitle::new("Purge verification page").expect("title");
        let captured = library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: PageId::new(70).expect("page"),
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(700).expect("revision"),
                    parent_id: None,
                    timestamp: "2026-08-24T00:00:00Z",
                    author: Some("Fixture author"),
                    author_id: Some(1),
                    comment: Some("purge verification fixture"),
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"exclusive purge verification payload",
                },
            )
            .expect("capture");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("start run")
            .status
            .run_id;
        library
            .complete_sync_run(run_id, None)
            .expect("complete run");
        library.append_sync_manifest(run_id).expect("sync manifest");
        library
            .tombstone_collection(collection_id)
            .expect("tombstone");
        if pack_object {
            library
                .pack_loose_objects()
                .expect("pack target")
                .expect("whole target pack");
        }
        let preview = library
            .preview_collection_purge(collection_id)
            .expect("preview");
        let receipt = library
            .authorize_collection_purge(
                collection_id,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorization");
        library
            .append_purge_manifest(receipt.purge_id)
            .expect("purge manifest");
        (directory, library, receipt.purge_id, captured.id)
    }

    fn advance_purge_to_state(library: &mut Library, purge_id: u64, expected: PurgeJournalState) {
        for _ in 0..16 {
            let state = library
                .purge_cleanup_progress(purge_id)
                .expect("cleanup progress")
                .state;
            if state == expected {
                return;
            }
            library
                .resume_purge_cleanup(purge_id)
                .expect("advance purge cleanup");
        }
        panic!("purge {purge_id} did not reach {expected:?}");
    }

    fn metadata_fixture() -> (TempDir, Library) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "fixture")
            .expect("collection");
        let title = PageTitle::new("Fixture page").expect("title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: PageId::new(10).expect("page"),
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(20).expect("revision"),
                    parent_id: Some(RevisionId::new(19).expect("uncaptured parent")),
                    timestamp: "2026-08-21T00:00:00Z",
                    author: Some("Fixture author"),
                    author_id: Some(1),
                    comment: Some("fixture"),
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"Fixture source",
                },
            )
            .expect("capture");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("start run")
            .status
            .run_id;
        library
            .complete_sync_run(run_id, Some("fixture-cursor"))
            .expect("complete run");
        library.append_sync_manifest(run_id).expect("manifest");

        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "INSERT INTO search_documents (
                    wiki_id, page_id, revision_id, transformer_version, indexed_at
                 ) VALUES (?1, ?2, ?3, ?4, 100)",
                params![
                    wiki_id.get(),
                    10_u64,
                    20_u64,
                    PLAIN_TEXT_TRANSFORMER_VERSION.as_str()
                ],
            )
            .expect("search document");
        let search_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO search_fts (
                    rowid, title, aliases, headings, body, categories, captions
                 ) VALUES (?1, 'Fixture page', '', '', 'Fixture source', '', '')",
                [search_id],
            )
            .expect("FTS row");
        (directory, library)
    }

    fn media_fixture() -> (TempDir, Library, ObjectId) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "media fixture")
            .expect("collection");
        let title = PageTitle::new("Media fixture page").expect("title");
        let page_id = PageId::new(40).expect("page");
        let revision_id = RevisionId::new(400).expect("revision");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id,
                    namespace: 0,
                    title: &title,
                    revision_id,
                    parent_id: None,
                    timestamp: "2026-08-22T00:00:00Z",
                    author: Some("Fixture author"),
                    author_id: Some(1),
                    comment: Some("media fixture"),
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"Media fixture source",
                },
            )
            .expect("capture revision");
        let file_title = PageTitle::new("File:Integrity fixture.png").expect("file title");
        let capture = ThumbnailCapture {
            media_id: MediaId::new(9_001).expect("media ID"),
            file_title: &file_title,
            source_sha1: "abcdef0123456789abcdef0123456789abcdef01",
            original_url: "https://upload.wikimedia.org/integrity-fixture.png",
            description_url: "https://commons.wikimedia.org/wiki/File:Integrity_fixture.png",
            author: "Fixture photographer",
            attribution: "Fixture photographer / Wikimedia Commons",
            license_name: "CC BY-SA 4.0",
            license_url: Some("https://creativecommons.org/licenses/by-sa/4.0/"),
            width: 1,
            height: 1,
            mime_type: ThumbnailMimeType::Png,
            captured_at: 1_776_000_000,
            source: VALID_PNG,
        };
        let stored = library
            .capture_revision_thumbnail(
                wiki_id,
                page_id,
                revision_id,
                ThumbnailPolicy::default(),
                &capture,
                RevisionMediaPlacement {
                    index: 0,
                    kind: MediaPlacementKind::Lead,
                    caption: Some("Fixture caption"),
                    alt_text: Some("Fixture alternative text"),
                },
            )
            .expect("capture thumbnail");
        (directory, library, stored.id)
    }

    fn manifested_media_fixture() -> (TempDir, Library, ObjectId) {
        let (directory, mut library, object_id) = media_fixture();
        let run_id = library
            .start_or_resume_sync_run(
                wikisync_core::WikiId::new(1).expect("wiki"),
                Some(CollectionId::new(1).expect("collection")),
                SyncRunKind::Bootstrap,
                100,
            )
            .expect("start media run")
            .status
            .run_id;
        library
            .complete_sync_run(run_id, None)
            .expect("complete media run");
        library
            .append_sync_manifest(run_id)
            .expect("media manifest");
        (directory, library, object_id)
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
    fn full_verification_replays_schema_v4_claim_shards() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = StoreConfig::default()
            .with_max_manifest_shard_entries(2)
            .expect("tiny shards");
        let mut library = Library::open_with_config(directory.path(), config).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "sharded claims")
            .expect("collection");
        for value in 1..=3_u64 {
            let title = PageTitle::new(format!("Sharded claim {value}")).expect("title");
            library
                .capture_current_revision(
                    wiki_id,
                    collection_id,
                    &CurrentRevisionCapture {
                        page_id: PageId::new(value).expect("page"),
                        namespace: 0,
                        title: &title,
                        revision_id: RevisionId::new(value * 10).expect("revision"),
                        parent_id: None,
                        timestamp: "2026-08-30T10:00:00Z",
                        author: None,
                        author_id: None,
                        comment: None,
                        minor: false,
                        upstream_sha1: None,
                        content_model: "wikitext",
                        source: title.as_str().as_bytes(),
                    },
                )
                .expect("capture");
        }
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("run")
            .status
            .run_id;
        library.complete_sync_run(run_id, None).expect("complete");
        let stored = library.append_sync_manifest(run_id).expect("manifest");
        assert_eq!(stored.shards.len(), 4);

        let report = verify_library(&library, VerificationScope::Full).expect("verification");
        assert_eq!(report.finding_count, 0, "{:?}", report.findings);
        assert_eq!(report.manifest_revision_claims_examined, 3);
        assert_eq!(report.manifest_page_head_claims_examined, 3);
    }

    #[test]
    fn authenticated_purge_event_and_exact_pending_journal_verify_cleanly() {
        let (_directory, library, purge_id, _object_id) = authenticated_purge_fixture();

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(report.manifests_at_start, 2);
        assert_eq!(report.purge_events_examined, 1);
        assert_eq!(report.purges_pending_cleanup, 1);
        assert_eq!(report.authorized_absences_verified, 0);
        assert_eq!(report.finding_count, 0, "{:?}", report.findings);
        assert!(report.is_verified_since_capture());
        assert_eq!(
            library
                .purge_verification_snapshot(purge_id)
                .expect("snapshot")
                .state,
            PurgeJournalState::Authorized
        );
    }

    #[test]
    fn authenticated_purge_event_requires_its_exact_durable_journal() {
        let (_directory, library, purge_id, _object_id) = authenticated_purge_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "DELETE FROM purge_operations WHERE purge_id = ?1",
                [purge_id],
            )
            .expect("remove journal fixture");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::PurgeJournalMissing
                && finding.manifest_sequence == Some(2)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn authenticated_purge_event_rejects_mismatched_journal_binding() {
        let (_directory, library, purge_id, _object_id) = authenticated_purge_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "UPDATE purge_operations
                 SET collection_name = 'Changed purge name',
                     acknowledged_collection_name = 'Changed purge name'
                 WHERE purge_id = ?1",
                [purge_id],
            )
            .expect("change journal binding");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::PurgeJournalMismatch
                && finding.manifest_sequence == Some(2)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn authenticated_purge_event_rejects_changed_collection_tombstone() {
        let (_directory, library, purge_id, _object_id) = authenticated_purge_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        let collection_id: i64 = connection
            .query_row(
                "SELECT collection_id FROM purge_operations WHERE purge_id = ?1",
                [purge_id],
                |row| row.get(0),
            )
            .expect("purge collection");
        connection
            .execute(
                "UPDATE collections SET generation = generation + 1
                 WHERE collection_id = ?1",
                [collection_id],
            )
            .expect("change retained tombstone");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::PurgeJournalMismatch
                && finding.manifest_sequence == Some(2)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn authenticated_purge_event_rejects_changed_inventory_rows() {
        let (_directory, library, purge_id, _object_id) = authenticated_purge_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "UPDATE purge_objects
                 SET uncompressed_length = uncompressed_length + 1
                 WHERE purge_id = ?1",
                [purge_id],
            )
            .expect("change journal inventory");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::PurgeInventoryMismatch
                && finding.manifest_sequence == Some(2)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn absent_payload_without_positive_authorized_absence_is_unexplained_loss() {
        let (directory, library, _purge_id, object_id) = authenticated_purge_fixture();
        let encoded = object_id.to_string();
        let digest = encoded.strip_prefix("b3:").expect("object prefix");
        let loose_path = directory
            .path()
            .join("objects/loose/b3")
            .join(&digest[..2])
            .join(&digest[2..4])
            .join(digest);
        fs::remove_file(loose_path).expect("remove unauthorized payload fixture");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::UnexplainedObjectLoss
                && finding.object_id == Some(object_id)
        }));
        assert_eq!(report.authorized_absences_verified, 0);
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn loose_cleanup_authorizes_exact_absence_while_pending_and_after_completion() {
        let (_directory, mut library, purge_id, object_id) = authenticated_purge_fixture();
        advance_purge_to_state(&mut library, purge_id, PurgeJournalState::Cleaning);

        let cleaning = verify_library(&library, VerificationScope::Full).expect("cleaning verify");
        assert_eq!(cleaning.authorized_absences_verified, 1);
        assert_eq!(cleaning.purges_pending_cleanup, 1);
        assert_eq!(cleaning.finding_count, 0, "{:?}", cleaning.findings);
        assert!(cleaning.is_verified_since_capture());
        assert!(matches!(
            library.read_object(object_id),
            Err(StoreError::ObjectNotFound(id)) if id == object_id
        ));

        advance_purge_to_state(&mut library, purge_id, PurgeJournalState::Succeeded);
        let completed =
            verify_library(&library, VerificationScope::Full).expect("completed verify");
        assert_eq!(completed.authorized_absences_verified, 1);
        assert_eq!(completed.purges_pending_cleanup, 0);
        assert_eq!(completed.finding_count, 0, "{:?}", completed.findings);
        assert!(completed.is_verified_since_capture());
    }

    #[test]
    fn whole_pack_cleanup_authorizes_exact_absence_after_retirement() {
        let (_directory, mut library, purge_id, object_id) =
            authenticated_purge_fixture_with_storage(true);
        advance_purge_to_state(&mut library, purge_id, PurgeJournalState::Cleaning);

        let cleaning = verify_library(&library, VerificationScope::Full).expect("cleaning verify");
        assert_eq!(cleaning.authorized_absences_verified, 1);
        assert_eq!(cleaning.purges_pending_cleanup, 1);
        assert_eq!(cleaning.finding_count, 0, "{:?}", cleaning.findings);

        advance_purge_to_state(&mut library, purge_id, PurgeJournalState::Succeeded);
        let completed =
            verify_library(&library, VerificationScope::Full).expect("completed verify");
        assert_eq!(completed.authorized_absences_verified, 1);
        assert_eq!(completed.purges_pending_cleanup, 0);
        assert_eq!(completed.finding_count, 0, "{:?}", completed.findings);
        assert!(completed.is_verified_since_capture());
        assert!(matches!(
            library.read_object(object_id),
            Err(StoreError::ObjectNotFound(id)) if id == object_id
        ));
    }

    #[test]
    fn superseded_absence_requires_readable_bytes_and_preserves_purge_history() {
        let (_directory, mut library, purge_id, object_id) = authenticated_purge_fixture();
        advance_purge_to_state(&mut library, purge_id, PurgeJournalState::Succeeded);
        let wiki_id = wikisync_core::WikiId::new(1).expect("wiki");
        let retained = library
            .create_explicit_collection(wiki_id, "Post-purge retained collection")
            .expect("retained collection");
        let title = PageTitle::new("Post-purge retained page").expect("title");
        let restored = library
            .capture_current_revision(
                wiki_id,
                retained,
                &CurrentRevisionCapture {
                    page_id: PageId::new(71).expect("page"),
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(701).expect("revision"),
                    parent_id: None,
                    timestamp: "2026-08-24T02:00:00Z",
                    author: Some("Fixture author"),
                    author_id: Some(1),
                    comment: Some("restored after purge"),
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"exclusive purge verification payload",
                },
            )
            .expect("restore captured payload");
        assert_eq!(restored.id, object_id);
        assert!(
            library
                .purge_authorized_absence(object_id)
                .expect("active absence lookup")
                .is_none()
        );
        assert!(
            library
                .purge_authorized_absence_for_purge(purge_id, object_id)
                .expect("historical absence lookup")
                .expect("historical absence")
                .superseded_at
                .is_some()
        );

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(report.objects_verified, report.objects_at_start);
        assert_eq!(report.authorized_absences_verified, 0);
        assert_eq!(report.purges_pending_cleanup, 0);
        assert_eq!(report.finding_count, 0, "{:?}", report.findings);
        assert!(report.is_verified_since_capture());
    }

    #[test]
    fn superseded_absence_cannot_explain_later_payload_loss() {
        let (directory, mut library, purge_id, object_id) = authenticated_purge_fixture();
        advance_purge_to_state(&mut library, purge_id, PurgeJournalState::Succeeded);
        library
            .put_bytes(
                ObjectKind::Wikitext,
                b"exclusive purge verification payload",
            )
            .expect("restore payload");
        let encoded = object_id.to_string();
        let digest = encoded.strip_prefix("b3:").expect("object prefix");
        fs::remove_file(
            directory
                .path()
                .join("objects/loose/b3")
                .join(&digest[..2])
                .join(&digest[2..4])
                .join(digest),
        )
        .expect("remove restored payload");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(report.authorized_absences_verified, 0);
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::UnexplainedObjectLoss
                && finding.object_id == Some(object_id)
        }));
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == VerificationFindingKind::PurgeCleanupMismatch)
        );
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn missing_authorized_absence_row_fails_cleanup_and_remains_unexplained() {
        let (_directory, mut library, purge_id, object_id) = authenticated_purge_fixture();
        advance_purge_to_state(&mut library, purge_id, PurgeJournalState::Cleaning);
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "DELETE FROM purge_authorized_absences WHERE object_id = ?1",
                [object_id.to_string()],
            )
            .expect("remove absence evidence");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(report.authorized_absences_verified, 0);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == VerificationFindingKind::PurgeCleanupMismatch)
        );
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::UnexplainedObjectLoss
                && finding.object_id == Some(object_id)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn succeeded_cleanup_rejects_tampered_completion_accounting() {
        let (_directory, mut library, purge_id, object_id) = authenticated_purge_fixture();
        advance_purge_to_state(&mut library, purge_id, PurgeJournalState::Succeeded);
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "UPDATE purge_cleanup_accounting
                 SET retired_file_bytes = retired_file_bytes + 1
                 WHERE purge_id = ?1",
                [purge_id],
            )
            .expect("tamper cleanup accounting");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(report.authorized_absences_verified, 0);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == VerificationFindingKind::PurgeCleanupMismatch)
        );
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::UnexplainedObjectLoss
                && finding.object_id == Some(object_id)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn purge_event_rejects_late_shared_reference_and_still_hashes_payload() {
        let (_directory, library, _purge_id, object_id) = authenticated_purge_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "INSERT INTO pages (
                    wiki_id, page_id, namespace, current_title, current_revision_id,
                    current_revision_time, state, first_captured_at, updated_at
                 ) VALUES (1, 71, 0, 'Late retained page', 701,
                           '2026-08-24T01:00:00Z', 'active', 1, 1)",
                [],
            )
            .expect("insert retained page");
        connection
            .execute(
                "INSERT INTO revisions (
                    wiki_id, revision_id, page_id, parent_revision_id, revision_time,
                    author_name, author_id, comment, is_minor, source_size,
                    upstream_sha1, content_model, content_object_id, captured_at
                 ) VALUES (1, 701, 71, NULL, '2026-08-24T01:00:00Z',
                           'Fixture author', 1, 'late shared reference', 0, 36,
                           NULL, 'wikitext', ?1, 1)",
                [object_id.to_string()],
            )
            .expect("insert retained revision");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::PurgeSharedReferenceViolation
        }));
        assert_eq!(report.objects_verified, report.objects_at_start);
        assert_eq!(report.authorized_absences_verified, 0);
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn manifest_claim_verification_accepts_a_historical_non_current_head() {
        let (_directory, library) = manifested_revision_history();

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(report.manifest_revision_claims_examined, 2);
        assert_eq!(report.manifest_page_head_claims_examined, 2);
        assert_eq!(report.finding_count, 0);
        assert!(report.is_verified_since_capture());
    }

    #[test]
    fn manifest_claim_scan_becomes_partial_when_retained_metadata_changes() {
        let (_directory, library) = manifested_revision_history();
        let mut report = verify_library(&library, VerificationScope::Full).expect("verification");
        let findings_before = report.finding_count;
        let change_counter = library
            .integrity_metadata_change_counter()
            .expect("change counter");
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "UPDATE pages SET current_title = 'Changed during claim scan'
                 WHERE wiki_id = 1 AND page_id = 10",
                [],
            )
            .expect("change retained metadata");

        record_manifest_claim_scan_stability(
            &library,
            change_counter,
            DEFAULT_MAX_RETAINED_FINDINGS,
            &mut report,
        )
        .expect("stability finding");

        assert_eq!(report.coverage, VerificationCoverage::Partial);
        assert_eq!(report.finding_count, findings_before + 1);
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestClaimsChangedDuringVerification
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn manifest_claim_verification_reports_missing_revision_and_page_rows() {
        let (_directory, library) = manifested_revision_history();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "DELETE FROM revisions WHERE wiki_id = 1 AND revision_id = 20",
                [],
            )
            .expect("delete manifested revision");
        connection
            .execute_batch("PRAGMA foreign_keys = OFF")
            .expect("disable foreign keys for corruption fixture");
        connection
            .execute("DELETE FROM pages WHERE wiki_id = 1 AND page_id = 10", [])
            .expect("delete manifested page");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestRevisionClaimMissing
                && finding.manifest_sequence == Some(1)
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestRevisionClaimPageMissing
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestPageHeadClaimPageMissing
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestPageHeadClaimRevisionMissing
                && finding.manifest_sequence == Some(1)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn manifest_claim_verification_reports_changed_object_and_page_ownership() {
        let (_directory, mut library) = manifested_revision_history();
        let replacement = library
            .put_bytes(ObjectKind::Wikitext, b"replacement canonical object")
            .expect("replacement object");
        let wiki_id = wikisync_core::WikiId::new(1).expect("wiki");
        let collection_id = CollectionId::new(1).expect("collection");
        let other_title = PageTitle::new("Other page").expect("title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: PageId::new(11).expect("other page"),
                    namespace: 0,
                    title: &other_title,
                    revision_id: RevisionId::new(30).expect("other revision"),
                    parent_id: None,
                    timestamp: "2026-08-23T00:00:00Z",
                    author: Some("Fixture author"),
                    author_id: Some(1),
                    comment: Some("other page"),
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"other page source",
                },
            )
            .expect("capture other page");
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "UPDATE revisions SET page_id = 11, content_object_id = ?1
                 WHERE wiki_id = 1 AND revision_id = 20",
                [replacement.id.to_string()],
            )
            .expect("tamper manifested revision");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestRevisionClaimPageMismatch
                && finding.manifest_sequence == Some(1)
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestRevisionClaimObjectMismatch
                && finding.manifest_sequence == Some(1)
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestPageHeadClaimRevisionPageMismatch
                && finding.manifest_sequence == Some(1)
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn full_verification_authenticates_latest_media_inventory_and_placements() {
        let (_directory, library, _object_id) = manifested_media_fixture();

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert_eq!(
            report.manifest_media_coverage,
            ManifestMediaCoverage::Complete
        );
        assert_eq!(report.manifest_media_snapshots_examined, 1);
        assert_eq!(report.finding_count, 0);
    }

    #[test]
    fn manifest_media_verification_detects_metadata_and_placement_tampering() {
        let (_directory, library, object_id) = manifested_media_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "UPDATE media SET author = 'Tampered author' WHERE source_media_id = 9001",
                [],
            )
            .expect("tamper media metadata");
        connection
            .execute(
                "UPDATE page_media SET caption = 'Tampered caption'
                 WHERE revision_id = 400 AND placement_index = 0",
                [],
            )
            .expect("tamper placement metadata");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestMediaTampered
                && finding.object_id == Some(object_id)
                && finding.manifest_sequence == Some(1)
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestMediaPlacementTampered
                && finding.object_id == Some(object_id)
                && finding.manifest_sequence == Some(1)
        }));
    }

    #[test]
    fn manifest_media_verification_detects_deletion_swapping_and_added_inventory() {
        let (_directory, library, object_id) = manifested_media_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .pragma_update(None, "foreign_keys", false)
            .expect("disable fixture foreign keys");
        connection
            .execute(
                "UPDATE page_media SET source_media_id = 9002
                 WHERE revision_id = 400 AND placement_index = 0",
                [],
            )
            .expect("swap placement target");

        let swapped = verify_library(&library, VerificationScope::Full).expect("swapped");
        assert!(swapped.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestMediaPlacementSwapped
                && finding.manifest_sequence == Some(1)
        }));
        assert!(swapped.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestMediaDeleted
                && finding.object_id == Some(object_id)
        }));

        connection
            .execute(
                "UPDATE page_media SET source_media_id = 9001
                 WHERE revision_id = 400 AND placement_index = 0",
                [],
            )
            .expect("restore placement target");
        connection
            .execute(
                "INSERT INTO page_media (
                    wiki_id, revision_id, placement_index, source_media_id,
                    source_sha1, content_object_id, placement_kind, caption, alt_text
                 ) SELECT wiki_id, revision_id, 1, source_media_id, source_sha1,
                          content_object_id, 'inline', 'Added placement', NULL
                   FROM page_media WHERE revision_id = 400 AND placement_index = 0",
                [],
            )
            .expect("add unmanifested placement");
        let added = verify_library(&library, VerificationScope::Full).expect("added inventory");
        assert!(added.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestMediaInventoryChanged
                && finding.manifest_sequence == Some(1)
        }));

        connection
            .execute("DELETE FROM page_media WHERE revision_id = 400", [])
            .expect("delete placements");
        connection
            .execute("DELETE FROM media WHERE source_media_id = 9001", [])
            .expect("delete media");
        let deleted = verify_library(&library, VerificationScope::Full).expect("deleted");
        assert!(deleted.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestMediaDeleted
                && finding.object_id == Some(object_id)
        }));
        assert!(deleted.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::ManifestMediaPlacementDeleted
                && finding.object_id == Some(object_id)
        }));
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
    fn ed25519_trusted_head_round_trips_and_authenticates_full_verification() {
        let (_directory, library) = manifested_library(3);
        let signing_key = ManifestSigningKey::generate().expect("signing key");
        let trusted_head =
            sign_current_manifest_head(&library, &signing_key).expect("trusted head");
        let exported = trusted_head.to_canonical_json().expect("canonical JSON");
        let imported =
            TrustedManifestHead::from_canonical_json(&exported).expect("import trusted head");

        assert_eq!(imported, trusted_head);
        assert_eq!(imported.sequence, 3);
        assert_eq!(imported.public_key().len(), ED25519_PUBLIC_KEY_BYTES);
        assert_eq!(imported.signature().len(), ED25519_SIGNATURE_BYTES);
        assert!(
            !String::from_utf8(exported)
                .expect("UTF-8 JSON")
                .contains(&encode_hex(&signing_key.to_pkcs8_bytes()))
        );

        let report = verify_library_against_trusted_head(
            &library,
            VerificationOptions::new(VerificationScope::Full),
            &imported,
        )
        .expect("authenticated verification");

        assert_eq!(report.finding_count, 0);
        assert!(report.trusted_head_authenticated);
        assert!(report.is_authenticated_against_trusted_head());
    }

    #[test]
    fn signature_tampering_is_distinct_from_local_manifest_integrity() {
        let (_directory, library) = manifested_library(1);
        let signing_key = ManifestSigningKey::generate().expect("signing key");
        let mut trusted_head =
            sign_current_manifest_head(&library, &signing_key).expect("trusted head");
        trusted_head.signature[0] ^= 0x80;

        let report = verify_library_against_trusted_head(
            &library,
            VerificationOptions::new(VerificationScope::Full),
            &trusted_head,
        )
        .expect("verification report");

        assert!(!report.is_verified_since_capture());
        assert!(!report.trusted_head_authenticated);
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::TrustedHeadSignatureInvalid
        }));
    }

    #[test]
    fn a_valid_older_anchor_does_not_authenticate_an_advanced_library() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let first_run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start first")
            .status
            .run_id;
        library
            .complete_sync_run(first_run, None)
            .expect("complete first");
        library
            .append_sync_manifest(first_run)
            .expect("first manifest");
        let signing_key = ManifestSigningKey::generate().expect("signing key");
        let older_anchor =
            sign_current_manifest_head(&library, &signing_key).expect("older anchor");

        let second_run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 200)
            .expect("start second")
            .status
            .run_id;
        library
            .complete_sync_run(second_run, None)
            .expect("complete second");
        library
            .append_sync_manifest(second_run)
            .expect("second manifest");

        let report = verify_library_against_trusted_head(
            &library,
            VerificationOptions::new(VerificationScope::Full),
            &older_anchor,
        )
        .expect("verification report");

        assert!(!report.trusted_head_authenticated);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == VerificationFindingKind::TrustedHeadMismatch)
        );
    }

    #[test]
    fn trusted_head_import_is_bounded_strict_and_canonical() {
        let (_directory, library) = manifested_library(1);
        let signing_key = ManifestSigningKey::generate().expect("signing key");
        let trusted_head =
            sign_current_manifest_head(&library, &signing_key).expect("trusted head");
        let mut noncanonical = trusted_head.to_canonical_json().expect("JSON");
        noncanonical.push(b'\n');

        assert!(matches!(
            TrustedManifestHead::from_canonical_json(&noncanonical),
            Err(TrustedHeadError::InvalidAnchor(_))
        ));
        assert!(matches!(
            TrustedManifestHead::from_canonical_json(&vec![b' '; MAX_TRUSTED_HEAD_BYTES + 1]),
            Err(TrustedHeadError::AnchorTooLarge)
        ));
        assert!(matches!(
            verify_library_against_trusted_head(
                &library,
                VerificationOptions::new(VerificationScope::Quick),
                &trusted_head,
            ),
            Err(VerificationError::TrustedHeadRequiresFullVerification)
        ));
    }

    #[test]
    fn signing_key_pkcs8_round_trip_preserves_public_identity() {
        let (_directory, library) = manifested_library(1);
        let original = ManifestSigningKey::generate().expect("signing key");
        let imported =
            ManifestSigningKey::from_pkcs8(&original.to_pkcs8_bytes()).expect("import signing key");
        let original_head = sign_current_manifest_head(&library, &original).expect("original head");
        let imported_head = sign_current_manifest_head(&library, &imported).expect("imported head");

        assert_eq!(original_head, imported_head);
        assert!(format!("{original:?}").contains("REDACTED"));
        assert!(!format!("{original:?}").contains(&encode_hex(&original.to_pkcs8_bytes())));
    }

    #[test]
    fn signing_refuses_an_empty_or_gapped_manifest_history() {
        let empty_directory = tempfile::tempdir().expect("empty temporary directory");
        let empty = Library::open(empty_directory.path()).expect("empty library");
        let signing_key = ManifestSigningKey::generate().expect("signing key");
        assert!(matches!(
            sign_current_manifest_head(&empty, &signing_key),
            Err(TrustedHeadError::EmptyManifestHistory)
        ));

        let (directory, gapped) = manifested_library(3);
        fs::remove_file(directory.path().join("manifests/000000000002.json"))
            .expect("remove middle manifest");
        assert!(matches!(
            sign_current_manifest_head(&gapped, &signing_key),
            Err(TrustedHeadError::InvalidManifestHistory(_))
        ));
    }

    #[test]
    fn full_verification_scans_every_current_metadata_kind_across_bounded_pages() {
        let (_directory, library) = metadata_fixture();
        let options = VerificationOptions {
            page_size: 1,
            ..VerificationOptions::new(VerificationScope::Full)
        };

        let report = verify_library_with_options(&library, options).expect("verification");

        assert_eq!(report.metadata_records_at_start, 5);
        assert_eq!(report.metadata_records_examined, 5);
        assert_eq!(report.metadata_records_at_end, 5);
        assert_eq!(report.finding_count, 0);
        assert!(report.is_verified_since_capture());

        let mut cursor = None;
        let mut subjects = Vec::new();
        loop {
            let page = library
                .integrity_metadata_records_after(cursor, 1)
                .expect("metadata page");
            let Some(record) = page.into_iter().next() else {
                break;
            };
            cursor = Some(record.cursor().expect("cursor"));
            subjects.push(record.subject);
        }
        assert!(matches!(
            subjects.as_slice(),
            [
                IntegrityMetadataSubject::Revision { .. },
                IntegrityMetadataSubject::Page { .. },
                IntegrityMetadataSubject::Checkpoint { .. },
                IntegrityMetadataSubject::SearchDocument { .. },
                IntegrityMetadataSubject::SearchFtsRow { .. }
            ]
        ));
    }

    #[test]
    fn full_verification_scans_valid_media_and_placements_in_bounded_pages() {
        let (_directory, library, media_object_id) = media_fixture();
        let report = verify_library_with_options(
            &library,
            VerificationOptions {
                page_size: 1,
                ..VerificationOptions::new(VerificationScope::Full)
            },
        )
        .expect("verification");

        assert_eq!(report.objects_at_start, 2);
        assert_eq!(report.objects_verified, 2);
        assert_eq!(report.metadata_records_at_start, 4);
        assert_eq!(report.metadata_records_examined, 4);
        assert_eq!(report.metadata_records_at_end, 4);
        assert_eq!(report.finding_count, 0);
        assert!(report.is_verified_since_capture());
        assert_eq!(
            library.read_object(media_object_id).expect("media object"),
            VALID_PNG
        );
    }

    #[test]
    fn full_verification_rejects_malformed_media_and_out_of_contract_metadata() {
        let (_directory, mut library, _media_object_id) = media_fixture();
        let malformed = library
            .put_bytes(
                ObjectKind::Media,
                b"\x89PNG\r\n\x1a\nstructurally incomplete raster",
            )
            .expect("store independently hash-valid malformed media object");
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "INSERT INTO media (
                    wiki_id, source_media_id, source_sha1, file_title, original_url,
                    description_url, author, attribution, license_name, license_url,
                    width, height, mime_type, captured_at, content_object_id
                 ) VALUES (
                    1, 9002, 'abcdefabcdefabcdefabcdefabcdefabcdefabcd',
                    'File:Malformed.png', 'https://upload.wikimedia.org/malformed.png',
                    'https://commons.wikimedia.org/wiki/File:Malformed.png',
                    'Fixture author', 'Fixture attribution', 'CC0', NULL,
                    1, 1, 'image/png', 1776000001, ?1
                 )",
                [malformed.id.to_string()],
            )
            .expect("insert corrupt fixture through raw metadata boundary");
        connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("disable fixture check constraints");
        connection
            .execute(
                "UPDATE media
                 SET author = 'control' || char(10) || 'metadata', width = 4097
                 WHERE source_media_id = 9001",
                [],
            )
            .expect("inject out-of-contract metadata");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::MediaMetadataMismatch
                && finding.object_id == Some(malformed.id)
                && finding
                    .message
                    .contains("complete bounded passive-raster validation")
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::MediaMetadataMismatch
                && matches!(
                    finding.metadata_subject,
                    Some(IntegrityMetadataSubject::Media {
                        source_media_id: 9001,
                        ..
                    })
                )
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn full_verification_reports_missing_and_wrong_kind_media_objects() {
        let (_directory, library, media_object_id) = media_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .pragma_update(None, "foreign_keys", false)
            .expect("disable fixture foreign keys");
        connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("disable fixture check constraints");
        let absent_object = format!("b3:{}", "00".repeat(32));
        connection
            .execute(
                "UPDATE media SET content_object_id = ?1 WHERE source_media_id = 9001",
                [&absent_object],
            )
            .expect("break media object pointer");

        let missing = verify_library(&library, VerificationScope::Full).expect("verification");
        assert!(missing.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::MediaObjectUnreachable
                && finding.object_id == absent_object.parse().ok()
                && matches!(
                    finding.metadata_subject,
                    Some(IntegrityMetadataSubject::Media {
                        wiki_id: 1,
                        source_media_id: 9001,
                        ..
                    })
                )
        }));

        connection
            .execute(
                "UPDATE media SET content_object_id = ?1, mime_type = 'image/jpeg', width = 0
                 WHERE source_media_id = 9001",
                [media_object_id.to_string()],
            )
            .expect("restore pointer and mismatch MIME");
        connection
            .execute(
                "UPDATE content_objects SET object_kind = 'wikitext' WHERE object_id = ?1",
                [media_object_id.to_string()],
            )
            .expect("break media object kind");

        let mismatched = verify_library(&library, VerificationScope::Full).expect("verification");
        assert!(mismatched.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::MediaObjectKindMismatch
                && finding.object_id == Some(media_object_id)
        }));
        assert!(mismatched.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::MediaMetadataMismatch
                && finding.object_id == Some(media_object_id)
        }));
        assert!(!mismatched.is_verified_since_capture());

        connection
            .execute(
                "UPDATE content_objects SET object_kind = 'media' WHERE object_id = ?1",
                [media_object_id.to_string()],
            )
            .expect("restore media object kind");
        connection
            .execute(
                "UPDATE media SET width = 1 WHERE source_media_id = 9001",
                [],
            )
            .expect("restore dimensions while retaining mismatched MIME");
        let signature_mismatch =
            verify_library(&library, VerificationScope::Full).expect("verification");
        assert!(signature_mismatch.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::MediaMetadataMismatch
                && finding.object_id == Some(media_object_id)
                && finding
                    .message
                    .contains("complete bounded passive-raster validation")
        }));
    }

    #[test]
    fn full_verification_reports_media_revision_page_and_metadata_ownership() {
        let (_directory, library, _media_object_id) = media_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .pragma_update(None, "foreign_keys", false)
            .expect("disable fixture foreign keys");
        connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("disable fixture check constraints");
        connection
            .execute(
                "INSERT INTO page_media (
                    wiki_id, revision_id, placement_index, source_media_id,
                    source_sha1, content_object_id, placement_kind, caption, alt_text
                 ) SELECT wiki_id, 999, 1, source_media_id, source_sha1,
                          content_object_id, placement_kind, caption, alt_text
                   FROM page_media WHERE revision_id = 400 AND placement_index = 0",
                [],
            )
            .expect("insert unreachable revision placement");
        connection
            .execute("DELETE FROM pages WHERE page_id = 40", [])
            .expect("remove owning page");
        connection
            .execute(
                "UPDATE page_media SET source_media_id = 9999, placement_kind = 'unexpected'
                 WHERE revision_id = 400",
                [],
            )
            .expect("break media and placement metadata");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");
        let kinds = report
            .findings
            .iter()
            .map(|finding| finding.kind)
            .collect::<HashSet<_>>();
        assert!(kinds.contains(&VerificationFindingKind::MediaPlacementRevisionUnreachable));
        assert!(kinds.contains(&VerificationFindingKind::MediaPlacementPageUnreachable));
        assert!(kinds.contains(&VerificationFindingKind::MediaPlacementMediaUnreachable));
        assert!(kinds.contains(&VerificationFindingKind::MediaPlacementMetadataMismatch));
        assert!(
            report
                .findings
                .iter()
                .filter(|finding| {
                    matches!(
                        finding.kind,
                        VerificationFindingKind::MediaPlacementRevisionUnreachable
                            | VerificationFindingKind::MediaPlacementPageUnreachable
                            | VerificationFindingKind::MediaPlacementMediaUnreachable
                            | VerificationFindingKind::MediaPlacementMetadataMismatch
                    )
                })
                .all(|finding| {
                    matches!(
                        finding.metadata_subject,
                        Some(IntegrityMetadataSubject::PageMedia { .. })
                    )
                })
        );
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn full_verification_reports_media_hash_failure_with_metadata_subject() {
        let (directory, library, media_object_id) = media_fixture();
        let encoded = media_object_id.to_string();
        let digest = encoded.strip_prefix("b3:").expect("object prefix");
        let loose_path = directory
            .path()
            .join("objects/loose/b3")
            .join(&digest[..2])
            .join(&digest[2..4])
            .join(digest);
        fs::write(loose_path, b"tampered compressed media").expect("tamper media object");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");
        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::MediaObjectUnreadable
                && finding.object_id == Some(media_object_id)
                && matches!(
                    finding.metadata_subject,
                    Some(IntegrityMetadataSubject::Media { .. })
                )
        }));
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn full_verification_reports_reachability_and_search_version_findings() {
        let (_directory, library) = metadata_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .pragma_update(None, "foreign_keys", false)
            .expect("disable fixture foreign keys");
        let absent_object = format!("b3:{}", "00".repeat(32));
        connection
            .execute(
                "UPDATE revisions SET content_object_id = ?1 WHERE revision_id = 20",
                [absent_object],
            )
            .expect("break revision object pointer");
        connection
            .execute(
                "UPDATE pages SET current_revision_id = 999 WHERE page_id = 10",
                [],
            )
            .expect("break page head pointer");
        connection
            .execute("UPDATE sync_checkpoints SET last_run_id = 999", [])
            .expect("break checkpoint run pointer");
        connection
            .execute(
                "UPDATE search_documents SET transformer_version = 'wikitext-plain-v0'",
                [],
            )
            .expect("stale search version");
        connection
            .execute("DELETE FROM search_fts", [])
            .expect("remove FTS pointer target");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");
        let kinds = report
            .findings
            .iter()
            .map(|finding| finding.kind)
            .collect::<HashSet<_>>();

        assert!(kinds.contains(&VerificationFindingKind::RevisionObjectUnreachable));
        assert!(kinds.contains(&VerificationFindingKind::PageHeadRevisionUnreachable));
        assert!(kinds.contains(&VerificationFindingKind::CheckpointRunUnreachable));
        assert!(kinds.contains(&VerificationFindingKind::SearchRevisionNotCurrent));
        assert!(kinds.contains(&VerificationFindingKind::SearchFtsRowMissing));
        assert!(kinds.contains(&VerificationFindingKind::SearchTransformerVersionMismatch));
        assert!(
            report
                .findings
                .iter()
                .filter(|finding| finding.metadata_subject.is_some())
                .all(|finding| finding.object_id.is_none())
        );
        assert!(!report.is_verified_since_capture());
    }

    #[test]
    fn full_verification_reports_orphan_fts_rows() {
        let (_directory, library) = metadata_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "INSERT INTO search_fts (
                    rowid, title, aliases, headings, body, categories, captions
                 ) VALUES (999, 'Orphan', '', '', '', '', '')",
                [],
            )
            .expect("orphan FTS row");

        let report = verify_library(&library, VerificationScope::Full).expect("verification");

        assert!(report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::SearchFtsRowOrphan
                && finding.metadata_subject
                    == Some(IntegrityMetadataSubject::SearchFtsRow { row_id: 999 })
        }));
    }

    #[test]
    fn quick_verification_does_not_scan_metadata_references() {
        let (_directory, library) = metadata_fixture();
        let connection = rusqlite::Connection::open(library.database_path()).expect("database");
        connection
            .execute(
                "UPDATE pages SET current_revision_id = 999 WHERE page_id = 10",
                [],
            )
            .expect("break page head pointer");

        let report = verify_library(&library, VerificationScope::Quick).expect("verification");

        assert_eq!(report.metadata_records_at_start, 0);
        assert_eq!(report.metadata_records_examined, 0);
        assert_eq!(report.metadata_records_at_end, 0);
        assert!(!report.findings.iter().any(|finding| {
            finding.kind == VerificationFindingKind::PageHeadRevisionUnreachable
        }));
        assert!(report.is_verified_since_capture());
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
