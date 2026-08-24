//! SQLite metadata and immutable content-object storage.
//!
//! Logical [`ObjectId`] values contain no physical location information. New bytes
//! are compressed into a temporary file, made durable, and atomically installed
//! before the SQLite transaction records their location.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use wikisync_content::{ThumbnailLimits, validate_thumbnail};
use wikisync_core::{
    CollectionBudget, CollectionId, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
    ImagePolicy, InclusionReason, MAX_THUMBNAILS_PER_REVISION, MediaId, PageId, PageTitle,
    RevisionId, ThumbnailPolicy, TitleSelection, UnixTimestamp, WikiId,
};

const MIGRATION_1: &str = include_str!("../migrations/0001_initial.sql");
const MIGRATION_2: &str = include_str!("../migrations/0002_capture.sql");
const MIGRATION_3: &str = include_str!("../migrations/0003_search.sql");
const MIGRATION_4: &str = include_str!("../migrations/0004_sync.sql");
const MIGRATION_5: &str = include_str!("../migrations/0005_packs.sql");
const MIGRATION_6: &str = include_str!("../migrations/0006_collections.sql");
const MIGRATION_7: &str = include_str!("../migrations/0007_schedules.sql");
const MIGRATION_8: &str = include_str!("../migrations/0008_manifest_configuration.sql");
const MIGRATION_9: &str = include_str!("../migrations/0009_network_transfer_policy.sql");
const MIGRATION_10: &str = include_str!("../migrations/0010_collection_status.sql");
const MIGRATION_11: &str = include_str!("../migrations/0011_pack_affinity.sql");
const MIGRATION_12: &str = include_str!("../migrations/0012_thumbnail_media.sql");
const MIGRATION_13: &str = include_str!("../migrations/0013_dump_imports.sql");
const MIGRATION_14: &str = include_str!("../migrations/0014_purge_journal.sql");
const OBJECT_DOMAIN: &[u8] = b"wikisync-object-v1\0";
const DATABASE_NAME: &str = "library.sqlite3";
const MANIFEST_DOMAIN: &[u8] = b"wikisync-manifest-v1\0";
const MANIFEST_MEDIA_DOMAIN: &[u8] = b"wikisync-manifest-media-v1\0";
const MANIFEST_MEDIA_PLACEMENT_DOMAIN: &[u8] = b"wikisync-manifest-media-placement-v1\0";
const MANIFEST_DIRECTORY: &str = "manifests";
const MANIFEST_SCHEMA_VERSION: u32 = 2;
const MANIFEST_FILENAME_DIGITS: usize = 12;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 100_000;
const MAX_MANIFEST_TEXT_BYTES: usize = 8 * 1024;
const COPY_BUFFER_SIZE: usize = 64 * 1024;
const PACK_MAGIC: &[u8; 8] = b"WSPACK1\0";
const INDEX_MAGIC: &[u8; 8] = b"WSINDEX1";
const PACK_HEADER_LENGTH: u64 = 8 + 8;
const PACK_ENTRY_HEADER_LENGTH: u64 = 1 + 1 + 2 + 32 + 32 + 8 + 8;
const INDEX_HEADER_LENGTH: u64 = 8 + 32 + 8;
const INDEX_ENTRY_LENGTH: u64 = 32 + 8 + 8;
const PACK_ENCODING_FULL: u8 = 1;
const PACK_ENCODING_DELTA: u8 = 2;
const DELTA_HEADER_LENGTH: usize = 16;
const MAX_DELTA_DEPTH: u16 = 8;
const DELTA_CANDIDATE_WINDOW: usize = 16;
const FULL_ENTRY_INTERVAL: usize = 16;
const MAX_DELTA_SIZE_RATIO: u64 = 2;
const MIN_DELTA_SAVINGS: usize = 16;
const MAX_SUPPORTED_PACK_OBJECTS: u32 = 1_000_000;
const MAX_MEDIA_METADATA_TEXT_BYTES: usize = 16 * 1024;
const MAX_MEDIA_SOURCE_HASH_BYTES: usize = 128;
const MAX_THUMBNAIL_BYTES_PER_PIXEL: u64 = 8;
const PURGE_PREVIEW_DOMAIN: &[u8] = b"wikisync-purge-preview-v1\0";

/// Default upper bound for one uncompressed canonical object (64 MiB).
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// Default maximum number of objects considered for one pack.
pub const DEFAULT_MAX_PACK_OBJECTS: u32 = 256;

/// Default maximum canonical input represented by one pack (512 MiB).
pub const DEFAULT_MAX_PACK_INPUT_BYTES: u64 = 512 * 1024 * 1024;

/// Default amount of already-committed source time revisited by an update (5 minutes).
pub const DEFAULT_SYNC_OVERLAP_SECONDS: u64 = 5 * 60;

/// Smallest supported recurring interval (one minute).
pub const MIN_SCHEDULE_INTERVAL_SECONDS: u32 = 60;

/// Largest supported recurring interval (366 days).
pub const MAX_SCHEDULE_INTERVAL_SECONDS: u32 = 366 * 24 * 60 * 60;

/// Largest supported schedule jitter (one day).
pub const MAX_SCHEDULE_JITTER_SECONDS: u32 = 24 * 60 * 60;

/// Largest due-schedule page accepted by [`Library::due_schedules`].
pub const MAX_DUE_SCHEDULES: u32 = 10_000;

/// Maximum manifest records returned by one bounded enumeration call.
pub const MAX_MANIFEST_PAGE_SIZE: u32 = 1_000;

/// Maximum metadata-reference records returned by one integrity enumeration call.
pub const MAX_INTEGRITY_METADATA_PAGE_SIZE: u32 = 1_000;

/// Maximum number of collection-exclusive canonical objects in one purge operation.
pub const MAX_PURGE_OBJECTS: u32 = 100_000;

/// Maximum number of active verified physical locations bound by one purge preview.
pub const MAX_PURGE_LOCATIONS: u32 = 1_000_000;

/// Maximum number of immutable packs affected by one purge preview.
pub const MAX_PURGE_AFFECTED_PACKS: u32 = 10_000;

/// Largest page accepted by [`Library::purge_objects_after`].
pub const MAX_PURGE_OBJECT_PAGE_SIZE: u32 = 1_000;

/// Default maximum number of source requests in flight across one library process.
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: u32 = 4;

/// Largest supported library-wide source-request concurrency.
pub const MAX_CONCURRENT_REQUESTS: u32 = 256;

/// Largest byte-per-second limit representable by the durable SQLite schema.
pub const MAX_DOWNLOAD_BYTES_PER_SECOND: u64 = i64::MAX as u64;

/// Durable library-wide policy for synchronization network transfers.
///
/// The default permits four concurrent requests, does not shape aggregate download
/// throughput, and permits transfers on metered networks. Metered-network avoidance
/// is only actionable where the caller can reliably identify that OS network state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkTransferPolicy {
    max_concurrent_requests: u32,
    max_download_bytes_per_second: Option<u64>,
    avoid_metered_networks: bool,
}

impl NetworkTransferPolicy {
    /// Builds a validated library-wide transfer policy.
    pub fn new(
        max_concurrent_requests: u32,
        max_download_bytes_per_second: Option<u64>,
        avoid_metered_networks: bool,
    ) -> Result<Self, StoreError> {
        if !(1..=MAX_CONCURRENT_REQUESTS).contains(&max_concurrent_requests) {
            return Err(StoreError::InvalidConfig(
                "maximum concurrent requests must be between 1 and 256",
            ));
        }
        if max_download_bytes_per_second
            .is_some_and(|bytes| bytes == 0 || bytes > MAX_DOWNLOAD_BYTES_PER_SECOND)
        {
            return Err(StoreError::InvalidConfig(
                "maximum download rate must be between 1 and 9,223,372,036,854,775,807 bytes per second when set",
            ));
        }
        Ok(Self {
            max_concurrent_requests,
            max_download_bytes_per_second,
            avoid_metered_networks,
        })
    }

    /// Returns the maximum number of concurrent source requests.
    #[must_use]
    pub const fn max_concurrent_requests(self) -> u32 {
        self.max_concurrent_requests
    }

    /// Returns the aggregate download-rate ceiling, or `None` when unlimited.
    #[must_use]
    pub const fn max_download_bytes_per_second(self) -> Option<u64> {
        self.max_download_bytes_per_second
    }

    /// Returns whether synchronization should wait while the OS reports metering.
    #[must_use]
    pub const fn avoid_metered_networks(self) -> bool {
        self.avoid_metered_networks
    }
}

impl Default for NetworkTransferPolicy {
    fn default() -> Self {
        Self {
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            max_download_bytes_per_second: None,
            avoid_metered_networks: false,
        }
    }
}

/// Configuration for a [`Library`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreConfig {
    max_object_bytes: u64,
    compression_level: i32,
    max_pack_objects: u32,
    max_pack_input_bytes: u64,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            compression_level: 3,
            max_pack_objects: DEFAULT_MAX_PACK_OBJECTS,
            max_pack_input_bytes: DEFAULT_MAX_PACK_INPUT_BYTES,
        }
    }
}

impl StoreConfig {
    /// Sets the maximum accepted uncompressed object length.
    pub fn with_max_object_bytes(mut self, bytes: u64) -> Result<Self, StoreError> {
        if bytes == 0 {
            return Err(StoreError::InvalidConfig(
                "maximum object size must be greater than zero",
            ));
        }
        self.max_object_bytes = bytes;
        Ok(self)
    }

    /// Sets the Zstandard compression level used for new loose objects.
    pub fn with_compression_level(mut self, level: i32) -> Result<Self, StoreError> {
        if !(-7..=22).contains(&level) {
            return Err(StoreError::InvalidConfig(
                "Zstandard compression level must be between -7 and 22",
            ));
        }
        self.compression_level = level;
        Ok(self)
    }

    /// Sets the maximum object count in one newly built pack.
    pub fn with_max_pack_objects(mut self, count: u32) -> Result<Self, StoreError> {
        if count == 0 || count > MAX_SUPPORTED_PACK_OBJECTS {
            return Err(StoreError::InvalidConfig(
                "maximum pack object count must be between 1 and 1,000,000",
            ));
        }
        self.max_pack_objects = count;
        Ok(self)
    }

    /// Sets the maximum sum of canonical object bytes considered for one pack.
    pub fn with_max_pack_input_bytes(mut self, bytes: u64) -> Result<Self, StoreError> {
        if bytes == 0 {
            return Err(StoreError::InvalidConfig(
                "maximum pack input size must be greater than zero",
            ));
        }
        self.max_pack_input_bytes = bytes;
        Ok(self)
    }
}

/// Canonical object categories. The category participates in logical identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ObjectKind {
    /// Exact MediaWiki revision source text.
    Wikitext,
    /// Captured binary media.
    Media,
}

impl ObjectKind {
    const fn identity_tag(self) -> u8 {
        match self {
            Self::Wikitext => 1,
            Self::Media => 2,
        }
    }

    fn from_identity_tag(tag: u8) -> Result<Self, StoreError> {
        match tag {
            1 => Ok(Self::Wikitext),
            2 => Ok(Self::Media),
            _ => Err(StoreError::CorruptPack("unknown object kind")),
        }
    }

    const fn database_value(self) -> &'static str {
        match self {
            Self::Wikitext => "wikitext",
            Self::Media => "media",
        }
    }

    const fn default_media_type(self) -> &'static str {
        match self {
            Self::Wikitext => "text/x-wiki; charset=utf-8",
            Self::Media => "application/octet-stream",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "wikitext" => Ok(Self::Wikitext),
            "media" => Ok(Self::Media),
            _ => Err(StoreError::CorruptMetadata("unknown object kind")),
        }
    }
}

/// A versioned, domain-separated BLAKE3 identity for canonical bytes.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    /// Computes an object identity without writing it.
    #[must_use]
    pub fn for_bytes(kind: ObjectKind, bytes: &[u8]) -> Self {
        let length = u64::try_from(bytes.len()).expect("slice length fits in u64");
        let mut hasher = object_hasher(kind, length);
        hasher.update(bytes);
        Self(*hasher.finalize().as_bytes())
    }

    /// Returns the raw 32-byte BLAKE3 digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn digest_hex(self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "b3:{}", self.digest_hex())
    }
}

impl FromStr for ObjectId {
    type Err = InvalidObjectId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest = value.strip_prefix("b3:").ok_or(InvalidObjectId)?;
        let hash = blake3::Hash::from_hex(digest).map_err(|_| InvalidObjectId)?;
        Ok(Self(*hash.as_bytes()))
    }
}

/// An object ID did not use the supported `b3:` form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidObjectId;

impl fmt::Display for InvalidObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("object ID must be b3: followed by 64 hexadecimal characters")
    }
}

impl Error for InvalidObjectId {}

/// Content identity of one canonical integrity-manifest body.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ManifestId([u8; 32]);

impl ManifestId {
    /// Returns the raw 32-byte BLAKE3 digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn for_body(body: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(MANIFEST_DOMAIN);
        hasher.update(&(body.len() as u64).to_le_bytes());
        hasher.update(body);
        Self(*hasher.finalize().as_bytes())
    }
}

impl fmt::Debug for ManifestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for ManifestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "b3:{}",
            blake3::Hash::from_bytes(self.0).to_hex()
        )
    }
}

impl FromStr for ManifestId {
    type Err = InvalidManifestId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest = value.strip_prefix("b3:").ok_or(InvalidManifestId)?;
        let hash = blake3::Hash::from_hex(digest).map_err(|_| InvalidManifestId)?;
        Ok(Self(*hash.as_bytes()))
    }
}

/// A manifest ID did not use the supported `b3:` form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidManifestId;

impl fmt::Display for InvalidManifestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("manifest ID must be b3: followed by 64 hexadecimal characters")
    }
}

impl Error for InvalidManifestId {}

/// One durable revision newly represented by this manifest chain entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestRevision {
    /// Stable page identity at the source.
    pub page_id: PageId,
    /// Stable source revision identity.
    pub revision_id: RevisionId,
    /// Immutable canonical content identity.
    pub content_object_id: ObjectId,
}

/// One resulting captured page head in the synchronization scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestPageHead {
    /// Stable page identity at the source.
    pub page_id: PageId,
    /// Captured head, absent when the source page has no locally captured head.
    pub revision_id: Option<RevisionId>,
}

/// One immutable captured media rendition authenticated by a media-aware manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestMedia {
    /// Stable MediaWiki file-page identity.
    pub media_id: MediaId,
    /// Exact upstream identity for the captured file version.
    pub source_sha1: String,
    /// Immutable canonical raster identity.
    pub content_object_id: ObjectId,
    /// Domain-separated identity of all durable source, attribution, raster, and
    /// capture-time metadata for this rendition.
    pub metadata_identity: String,
}

/// One revision-specific media placement authenticated by a media-aware manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestMediaPlacement {
    /// Owning immutable article revision.
    pub revision_id: RevisionId,
    /// Stable zero-based order within that revision.
    pub placement_index: u32,
    /// Stable MediaWiki file-page identity.
    pub media_id: MediaId,
    /// Exact upstream identity for the captured file version.
    pub source_sha1: String,
    /// Immutable canonical raster identity selected by the placement.
    pub content_object_id: ObjectId,
    /// Domain-separated identity of the placement kind, caption, alt text, and
    /// selected media identity.
    pub placement_identity: String,
}

/// Complete bounded media inventory and revision-placement snapshot for one scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestMediaSnapshot {
    /// Captured renditions in stable media/version/object order.
    pub inventory: Vec<ManifestMedia>,
    /// Revision placements in stable revision/index order.
    pub placements: Vec<ManifestMediaPlacement>,
}

/// Parsed, validated contents of one predecessor-linked synchronization manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncManifest {
    /// Strictly increasing append sequence, also encoded in the filename.
    pub sequence: u64,
    /// Identity of the exact canonical predecessor body.
    pub predecessor: Option<ManifestId>,
    /// Durable synchronization run represented by this manifest.
    pub run_id: u64,
    /// Local source identity.
    pub wiki_id: WikiId,
    /// Optional collection scope.
    pub collection_id: Option<CollectionId>,
    /// Stable source operation label.
    pub run_kind: SyncRunKind,
    /// Configured MediaWiki API endpoint observed for the run.
    pub source: String,
    /// Inclusive source discovery-window start.
    pub capture_started_at: u64,
    /// Source boundary made durable by the successful run.
    pub capture_completed_at: u64,
    /// Content identity of the durable collection/source configuration.
    pub configuration_hash: String,
    /// Durable revisions in scope that no predecessor manifest represented.
    pub introduced_revisions: Vec<ManifestRevision>,
    /// Resulting page heads in stable page-ID order.
    pub page_heads: Vec<ManifestPageHead>,
    /// Complete media state for this run scope. `None` means this is a readable
    /// schema-v1 manifest that predates authenticated media coverage.
    pub media_snapshot: Option<ManifestMediaSnapshot>,
}

/// One durably installed and identity-verified manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredManifest {
    /// Content identity recorded in and reproduced from the canonical file.
    pub id: ManifestId,
    /// Validated semantic contents.
    pub manifest: SyncManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestEnvelope {
    manifest_id: String,
    #[serde(flatten)]
    body: ManifestBody,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestBody {
    schema_version: u32,
    sequence: u64,
    predecessor: Option<String>,
    run_id: u64,
    wiki_id: u64,
    collection_id: Option<u64>,
    run_kind: String,
    source: String,
    capture_started_at: u64,
    capture_completed_at: u64,
    configuration_hash: String,
    introduced_revisions: Vec<ManifestRevisionWire>,
    page_heads: Vec<ManifestPageHeadWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media_inventory: Option<Vec<ManifestMediaWire>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media_placements: Option<Vec<ManifestMediaPlacementWire>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestRevisionWire {
    page_id: u64,
    revision_id: u64,
    content_object_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestPageHeadWire {
    page_id: u64,
    revision_id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestMediaWire {
    media_id: u64,
    source_sha1: String,
    content_object_id: String,
    metadata_identity: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestMediaPlacementWire {
    revision_id: u64,
    placement_index: u32,
    media_id: u64,
    source_sha1: String,
    content_object_id: String,
    placement_identity: String,
}

#[derive(Serialize)]
struct ManifestMediaIdentityBody<'a> {
    wiki_id: u64,
    media_id: u64,
    source_sha1: &'a str,
    content_object_id: &'a str,
    file_title: &'a str,
    original_url: &'a str,
    description_url: &'a str,
    author: &'a str,
    attribution: &'a str,
    license_name: &'a str,
    license_url: Option<&'a str>,
    width: i64,
    height: i64,
    mime_type: &'a str,
    captured_at: i64,
}

#[derive(Serialize)]
struct ManifestMediaPlacementIdentityBody<'a> {
    wiki_id: u64,
    revision_id: u64,
    placement_index: u32,
    media_id: u64,
    source_sha1: &'a str,
    content_object_id: &'a str,
    placement_kind: &'a str,
    caption: Option<&'a str>,
    alt_text: Option<&'a str>,
}

type ManifestConfigurationRow = (
    i64,
    String,
    Option<String>,
    Option<i64>,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// Metadata returned after an object is durably installed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredObject {
    /// Stable logical identity.
    pub id: ObjectId,
    /// Canonical object category.
    pub kind: ObjectKind,
    /// Length of the canonical, uncompressed bytes.
    pub uncompressed_length: u64,
}

/// Raster formats accepted for stable-v1 thumbnail capture.
///
/// Active source formats such as SVG are intentionally absent. A source adapter
/// must rasterize them before this storage boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailMimeType {
    /// JPEG raster bytes.
    Jpeg,
    /// PNG raster bytes.
    Png,
}

impl ThumbnailMimeType {
    /// Returns the stable MIME value stored in SQLite.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "image/jpeg" => Ok(Self::Jpeg),
            "image/png" => Ok(Self::Png),
            _ => Err(StoreError::CorruptMetadata(
                "unknown stored thumbnail MIME type",
            )),
        }
    }
}

/// Semantic placement of a captured thumbnail within one article revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaPlacementKind {
    /// The revision's lead/representative image.
    Lead,
    /// An image referenced within article content.
    Inline,
}

impl MediaPlacementKind {
    /// Returns the stable lowercase value stored in SQLite.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lead => "lead",
            Self::Inline => "inline",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "lead" => Ok(Self::Lead),
            "inline" => Ok(Self::Inline),
            _ => Err(StoreError::CorruptMetadata(
                "unknown stored media placement kind",
            )),
        }
    }
}

/// Canonical raster bytes and immutable source metadata for one file version.
#[derive(Clone, Debug)]
pub struct ThumbnailCapture<'a> {
    /// Stable MediaWiki file-page identity.
    pub media_id: MediaId,
    /// Canonical source file title.
    pub file_title: &'a PageTitle,
    /// Exact upstream SHA-1 metadata for this file version.
    pub source_sha1: &'a str,
    /// URL from which the raster bytes were observed.
    pub original_url: &'a str,
    /// Human-facing source description page used for attribution.
    pub description_url: &'a str,
    /// Upstream author/creator attribution.
    pub author: &'a str,
    /// Complete display-ready attribution text.
    pub attribution: &'a str,
    /// Upstream license name or identifier.
    pub license_name: &'a str,
    /// Upstream license URL, when one was supplied.
    pub license_url: Option<&'a str>,
    /// Raster width in pixels.
    pub width: u32,
    /// Raster height in pixels.
    pub height: u32,
    /// Validated passive raster MIME type.
    pub mime_type: ThumbnailMimeType,
    /// Local Unix capture time.
    pub captured_at: u64,
    /// Exact canonical raster bytes.
    pub source: &'a [u8],
}

/// Revision-specific caption, alternative text, and ordering for one thumbnail.
#[derive(Clone, Copy, Debug)]
pub struct RevisionMediaPlacement<'a> {
    /// Zero-based stable placement index within the revision.
    pub index: u32,
    /// Lead or inline placement.
    pub kind: MediaPlacementKind,
    /// Revision-specific visible caption.
    pub caption: Option<&'a str>,
    /// Revision-specific alternative text.
    pub alt_text: Option<&'a str>,
}

/// One immutable media version with its revision-specific placement metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredRevisionMedia {
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// Owning article revision.
    pub revision_id: RevisionId,
    /// Zero-based placement index within the revision.
    pub placement_index: u32,
    /// Lead or inline placement.
    pub placement_kind: MediaPlacementKind,
    /// Revision-specific caption.
    pub caption: Option<String>,
    /// Revision-specific alternative text.
    pub alt_text: Option<String>,
    /// Stable MediaWiki file-page identity.
    pub media_id: MediaId,
    /// Canonical source file title.
    pub file_title: PageTitle,
    /// Upstream SHA-1 metadata for this file version.
    pub source_sha1: String,
    /// URL from which the raster bytes were observed.
    pub original_url: String,
    /// Human-facing source description URL.
    pub description_url: String,
    /// Upstream author/creator attribution.
    pub author: String,
    /// Complete display-ready attribution text.
    pub attribution: String,
    /// Upstream license name or identifier.
    pub license_name: String,
    /// Upstream license URL, when supplied.
    pub license_url: Option<String>,
    /// Raster width in pixels.
    pub width: u32,
    /// Raster height in pixels.
    pub height: u32,
    /// Passive raster MIME type.
    pub mime_type: ThumbnailMimeType,
    /// Local Unix capture time.
    pub captured_at: u64,
    /// Stable BLAKE3 logical content identity.
    pub content_object_id: ObjectId,
}

/// Result of durably building, verifying, and activating one immutable pack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackSummary {
    /// Content-derived identity of the exact pack bytes.
    pub pack_id: String,
    /// Monotonically increasing physical representation generation.
    pub generation: u64,
    /// Number of logical objects represented by the pack.
    pub object_count: u64,
    /// Entries stored as independently compressed complete objects.
    pub full_entries: u64,
    /// Entries stored as bounded deltas from preceding entries.
    pub delta_entries: u64,
    /// On-disk pack length.
    pub pack_bytes: u64,
    /// On-disk index length.
    pub index_bytes: u64,
}

/// Metadata and canonical bytes committed for a page's observed current revision.
#[derive(Clone, Debug)]
pub struct CurrentRevisionCapture<'a> {
    /// Stable remote page identity.
    pub page_id: PageId,
    /// MediaWiki namespace number.
    pub namespace: i32,
    /// Canonical title returned by MediaWiki.
    pub title: &'a PageTitle,
    /// Stable remote revision identity.
    pub revision_id: RevisionId,
    /// Parent revision, which need not be captured by a current-and-future policy.
    pub parent_id: Option<RevisionId>,
    /// Canonical MediaWiki UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
    pub timestamp: &'a str,
    /// Public author name or IP, when available.
    pub author: Option<&'a str>,
    /// Public registered-user ID, when available.
    pub author_id: Option<u64>,
    /// Public edit comment, when available.
    pub comment: Option<&'a str>,
    /// Whether the edit is marked minor.
    pub minor: bool,
    /// Upstream MediaWiki SHA-1, when public.
    pub upstream_sha1: Option<&'a str>,
    /// Declared main-slot content model.
    pub content_model: &'a str,
    /// Exact canonical UTF-8 main-slot bytes.
    pub source: &'a [u8],
}

/// Metadata and canonical bytes for an additional revision of a captured page.
#[derive(Clone, Debug)]
pub struct RevisionCapture<'a> {
    /// Stable remote revision identity.
    pub revision_id: RevisionId,
    /// Parent revision, absent for the first revision in page history.
    pub parent_id: Option<RevisionId>,
    /// Canonical MediaWiki UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
    pub timestamp: &'a str,
    /// Public author name or IP, when available.
    pub author: Option<&'a str>,
    /// Public registered-user ID, when available.
    pub author_id: Option<u64>,
    /// Public edit comment, when available.
    pub comment: Option<&'a str>,
    /// Whether the edit is marked minor.
    pub minor: bool,
    /// Upstream MediaWiki SHA-1, when public.
    pub upstream_sha1: Option<&'a str>,
    /// Declared main-slot content model.
    pub content_model: &'a str,
    /// Exact canonical UTF-8 main-slot bytes.
    pub source: &'a [u8],
}

/// Persisted page head metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPage {
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// Stable remote page identity.
    pub page_id: PageId,
    /// MediaWiki namespace number.
    pub namespace: i32,
    /// Most recently observed canonical title.
    pub title: PageTitle,
    /// Most recently captured head revision.
    pub current_revision_id: Option<RevisionId>,
}

/// Persisted canonical revision reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredRevision {
    /// Stable remote revision identity.
    pub revision_id: RevisionId,
    /// Stable owning page identity.
    pub page_id: PageId,
    /// Parent revision identity, whether or not that revision is captured locally.
    pub parent_id: Option<RevisionId>,
    /// Canonical MediaWiki UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
    pub timestamp: String,
    /// Public author name or IP, when available.
    pub author: Option<String>,
    /// Public registered-user ID, when available.
    pub author_id: Option<u64>,
    /// Public edit comment, when available.
    pub comment: Option<String>,
    /// Whether MediaWiki marked the edit minor.
    pub minor: bool,
    /// Uncompressed canonical source length.
    pub source_size: u64,
    /// Upstream MediaWiki SHA-1, when public.
    pub upstream_sha1: Option<String>,
    /// Declared main-slot content model.
    pub content_model: String,
    /// Immutable logical content identity.
    pub content_object_id: ObjectId,
    /// Local Unix timestamp at which this revision was first captured.
    pub captured_at: u64,
}

/// Whether a collection still participates in membership resolution and synchronization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionStatus {
    /// The collection is configured and may be synchronized.
    Active,
    /// Tracking has stopped while all captured history and audit evidence is retained.
    Tombstoned,
}

impl CollectionStatus {
    /// Returns the stable lowercase value stored in SQLite and emitted in JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Tombstoned => "tombstoned",
        }
    }
}

/// Read-only collection summary used by local readers and status interfaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCollection {
    /// Local collection identity.
    pub collection_id: CollectionId,
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// User-visible collection name.
    pub name: String,
    /// Monotonic configuration/membership generation used to reject stale previews.
    pub generation: u64,
    /// Whether this collection is still tracked or retained only for audit/history.
    pub status: CollectionStatus,
    /// Unix timestamp when tracking stopped, if the collection is tombstoned.
    pub tombstoned_at: Option<u64>,
    /// Number of currently resolved pages in the collection.
    pub page_count: u64,
}

/// A configured MediaWiki source available to collection and GUI services.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredWiki {
    /// Stable local identity.
    pub wiki_id: WikiId,
    /// MediaWiki Action API endpoint.
    pub api_endpoint: String,
    /// User-facing source language code.
    pub language_code: String,
}

/// Complete persisted collection selection and retention configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCollectionConfiguration {
    /// Stable local collection identity.
    pub collection_id: CollectionId,
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// User-visible name.
    pub name: String,
    /// Monotonic configuration/membership generation used to reject stale previews.
    pub generation: u64,
    /// Whether this collection is still tracked or retained only for audit/history.
    pub status: CollectionStatus,
    /// Rule committed after any potentially large preview.
    pub rule: CollectionRule,
    /// Public revision-history capture policy.
    pub history_policy: HistoryPolicy,
    /// Hard collection limits.
    pub budget: CollectionBudget,
    /// Non-destructive behavior for pages removed by a dynamic rule.
    pub removal_policy: CollectionRemovalPolicy,
    /// Optional bounded raster-thumbnail capture policy.
    pub image_policy: ImagePolicy,
}

/// One stable page identity returned by a completed collection-rule preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCollectionMember {
    /// Remote stable page identity.
    pub page_id: PageId,
    /// MediaWiki namespace number.
    pub namespace: i32,
    /// Canonical title observed during resolution.
    pub title: PageTitle,
    /// Auditable reason this rule included the page.
    pub inclusion_reason: InclusionReason,
}

/// A fully resolved, bounded collection preview ready for one atomic commit.
#[derive(Clone, Copy, Debug)]
pub struct CollectionPreviewCommit<'a> {
    /// Selection rule used to produce the preview.
    pub rule: &'a CollectionRule,
    /// Revision-history policy to apply to every preview member.
    pub history_policy: HistoryPolicy,
    /// Hard limits checked before and during the transaction.
    pub budget: CollectionBudget,
    /// Policy for members absent from a later dynamic preview.
    pub removal_policy: CollectionRemovalPolicy,
    /// Complete resolved stable-page membership.
    pub members: &'a [ResolvedCollectionMember],
    /// Complete unresolved explicit/title-list inputs, assumed to be main namespace.
    pub missing_titles: &'a [PageTitle],
    /// Resolver prediction for canonical bytes, when one was available.
    pub predicted_canonical_bytes: Option<u64>,
}

/// Result of atomically replacing resolved membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MembershipCommit {
    /// Active members after the commit.
    pub active_members: u64,
    /// Former active members newly marked removed by this commit.
    pub removed_members: u64,
}

/// Current collection size information for budget decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollectionEstimate {
    /// Active stable page identities in the most recently committed resolution.
    pub resolved_page_count: u64,
    /// Deduplicated canonical bytes already captured for active member pages.
    pub current_canonical_bytes: u64,
    /// Optional pre-capture prediction supplied by a resolver.
    pub predicted_canonical_bytes: Option<u64>,
    /// Unix time of the resolver prediction, when one has been recorded.
    pub predicted_at: Option<u64>,
}

impl CollectionEstimate {
    /// Uses a prediction when available, but never reports less than already captured.
    #[must_use]
    pub fn expected_canonical_bytes(self) -> u64 {
        self.predicted_canonical_bytes
            .map_or(self.current_canonical_bytes, |predicted| {
                predicted.max(self.current_canonical_bytes)
            })
    }
}

/// A validated recurring interval used by a collection schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduleInterval(u32);

impl ScheduleInterval {
    /// Creates an interval between one minute and 366 days, inclusive.
    pub fn new(seconds: u32) -> Result<Self, StoreError> {
        if !(MIN_SCHEDULE_INTERVAL_SECONDS..=MAX_SCHEDULE_INTERVAL_SECONDS).contains(&seconds) {
            return Err(StoreError::InvalidConfig(
                "schedule interval must be between 60 and 31,622,400 seconds",
            ));
        }
        Ok(Self(seconds))
    }

    /// Returns the interval length in seconds.
    #[must_use]
    pub const fn seconds(self) -> u32 {
        self.0
    }
}

/// A validated UTC wall-clock time represented as seconds after midnight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DailyUtcTime(u32);

impl DailyUtcTime {
    /// Creates a UTC time from a value in `00:00:00` through `23:59:59`.
    pub fn new(seconds_after_midnight: u32) -> Result<Self, StoreError> {
        if seconds_after_midnight >= 24 * 60 * 60 {
            return Err(StoreError::InvalidConfig(
                "daily UTC time must be less than 86,400 seconds after midnight",
            ));
        }
        Ok(Self(seconds_after_midnight))
    }

    /// Returns the UTC wall-clock time as seconds after midnight.
    #[must_use]
    pub const fn seconds_after_midnight(self) -> u32 {
        self.0
    }
}

/// How a collection becomes eligible for scheduled synchronization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleCadence {
    /// The collection runs only after an explicit user request.
    Manual,
    /// The collection runs on a recurring elapsed-time interval.
    Interval(ScheduleInterval),
    /// The collection runs once per day at a UTC wall-clock time.
    DailyUtc(DailyUtcTime),
}

impl ScheduleCadence {
    /// Creates a validated interval cadence.
    pub fn interval(seconds: u32) -> Result<Self, StoreError> {
        ScheduleInterval::new(seconds).map(Self::Interval)
    }

    /// Creates a validated daily UTC cadence.
    pub fn daily_utc(seconds_after_midnight: u32) -> Result<Self, StoreError> {
        DailyUtcTime::new(seconds_after_midnight).map(Self::DailyUtc)
    }
}

/// Durable scheduling state for one collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollectionSchedule {
    /// Collection controlled by this schedule.
    pub collection_id: CollectionId,
    /// Configured cadence.
    pub cadence: ScheduleCadence,
    /// Maximum scheduler-selected delay after a nominal occurrence.
    pub jitter_seconds: u32,
    /// Whether automatic starts are temporarily disabled.
    pub paused: bool,
    /// Durable next eligible instant, absent for manual cadence.
    pub next_run_at: Option<u64>,
    /// Most recent instant atomically claimed for an automatic start.
    pub last_started_at: Option<u64>,
}

/// Logical object metadata used by paginated full-integrity verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalObject {
    /// Stable logical identity, kind, and uncompressed length.
    pub object: StoredObject,
    /// Persisted MIME type.
    pub media_type: String,
    /// Persisted metadata verification state.
    pub verification_state: ObjectVerificationState,
}

/// Deterministic, non-destructive preview of collection-exclusive canonical payload.
///
/// The preview retains logical metadata and hashes. Its byte estimate covers verified
/// loose copies and packs whose complete object inventory is exclusive to the target;
/// mixed packs require a later verified replacement-pack phase before their old bytes
/// become reclaimable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurgePreview {
    pub collection_id: CollectionId,
    pub collection_name: String,
    pub collection_generation: u64,
    pub tombstoned_at: u64,
    pub manifest_head_sequence: Option<u64>,
    pub manifest_head_id: Option<ManifestId>,
    pub fingerprint: String,
    pub object_count: u64,
    pub wikitext_object_count: u64,
    pub media_object_count: u64,
    pub logical_bytes: u64,
    pub reclaimable_bytes: u64,
    pub loose_object_count: u64,
    pub affected_pack_count: u64,
    pub whole_pack_count: u64,
    pub mixed_pack_count: u64,
}

/// One logical object durably selected by an authorized purge journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurgeObject {
    pub object: StoredObject,
}

/// Explicit confirmations required before a purge preview becomes durable work.
#[derive(Clone, Copy, Debug)]
pub struct PurgeAuthorization<'a> {
    /// Exact tombstoned collection name shown by the preview.
    pub collection_name: &'a str,
    /// Exact domain-separated preview fingerprint shown to the operator.
    pub preview_fingerprint: &'a str,
    /// Confirms that only local payload representations are in scope; audit metadata
    /// and hashes remain.
    pub payload_only_acknowledged: bool,
    /// Confirms that backups, snapshots, exports, and storage-device remnants are not
    /// erased by this operation.
    pub backups_not_erased_acknowledged: bool,
}

/// Durable authorization receipt for later restartable repacking and cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedPurge {
    pub purge_id: u64,
    pub preview: PurgePreview,
    pub authorized_at: u64,
}

#[derive(Clone, Debug)]
struct PurgeCandidate {
    id: ObjectId,
    kind: ObjectKind,
    uncompressed_length: u64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PurgeLocationFingerprint {
    object_id: ObjectId,
    storage_kind: String,
    encoding: String,
    relative_path: String,
    compressed_length: u64,
    base_object_id: Option<ObjectId>,
    pack_generation: Option<u64>,
    pack_id: Option<String>,
    pack_index_checksum: Option<String>,
    pack_offset: Option<u64>,
    delta_depth: Option<u16>,
}

#[derive(Clone, Debug)]
struct PurgePackSnapshot {
    pack_id: String,
    purged_object_count: u64,
    retained_object_count: u64,
    reclaimable_bytes: u64,
}

/// Persisted logical-object verification state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectVerificationState {
    /// Installation or verification is not complete.
    Pending,
    /// Canonical bytes were verified when recorded.
    Verified,
    /// A prior verification detected corruption.
    Corrupt,
}

/// Stable subject of one metadata-reference record examined by full verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntegrityMetadataSubject {
    /// One immutable captured revision, identified within its source wiki.
    Revision { wiki_id: u64, revision_id: u64 },
    /// One captured page, identified within its source wiki.
    Page { wiki_id: u64, page_id: u64 },
    /// One durable source checkpoint.
    Checkpoint { checkpoint_id: u64 },
    /// One rebuildable search-document pointer.
    SearchDocument { search_id: u64 },
    /// One contentless FTS row pointer.
    SearchFtsRow { row_id: i64 },
    /// One immutable captured media version, ordered by its stable SQLite row ID.
    Media {
        row_id: i64,
        wiki_id: i64,
        source_media_id: i64,
        /// Bounded prefix of the recorded upstream source hash.
        source_hash_prefix: String,
    },
    /// One ordered media placement on a captured revision.
    PageMedia {
        row_id: i64,
        wiki_id: i64,
        revision_id: i64,
        placement_index: i64,
    },
}

/// Metadata-reference invariant violated by a persisted record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityMetadataIssue {
    RevisionPageMissing,
    RevisionObjectMissing,
    RevisionParentWrongPage,
    RevisionParentSelfReference,
    PageHeadRevisionMissing,
    PageHeadRevisionWrongPage,
    CheckpointCollectionWikiMismatch,
    CheckpointRunMissing,
    CheckpointRunNotSucceeded,
    CheckpointRunScopeMismatch,
    CheckpointBoundaryMismatch,
    SearchPageMissing,
    SearchRevisionMissing,
    SearchRevisionWrongPage,
    SearchRevisionNotCurrent,
    SearchFtsRowMissing,
    SearchFtsRowOrphan,
    MediaObjectMissing,
    MediaObjectWrongKind,
    MediaMetadataInvalid,
    PageMediaRevisionMissing,
    PageMediaPageMissing,
    PageMediaMediaMissing,
    PageMediaMetadataInvalid,
}

/// Canonical-object reference exposed for bounded media byte verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrityMediaObject {
    pub object_id: ObjectId,
    /// Bounded MIME type recorded by the media metadata row.
    pub mime_type: String,
    /// Recorded decoded width, if it fits the stable-v1 representation.
    pub width: Option<u32>,
    /// Recorded decoded height, if it fits the stable-v1 representation.
    pub height: Option<u32>,
}

/// One bounded metadata record and every pointer inconsistency visible in the
/// current schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrityMetadataRecord {
    pub subject: IntegrityMetadataSubject,
    pub issues: Vec<IntegrityMetadataIssue>,
    /// Transformer version persisted for a search document, absent for other kinds.
    pub search_transformer_version: Option<String>,
    /// Parsed media-object candidate, absent for non-media or malformed references.
    pub media_object: Option<IntegrityMediaObject>,
}

/// Opaque keyset cursor for [`Library::integrity_metadata_records_after`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntegrityMetadataCursor {
    category: u8,
    first_key: i64,
    second_key: i64,
}

impl IntegrityMetadataRecord {
    /// Returns the opaque cursor used to continue after this exact record.
    pub fn cursor(&self) -> Result<IntegrityMetadataCursor, StoreError> {
        let (category, first_key, second_key) = match self.subject {
            IntegrityMetadataSubject::Revision {
                wiki_id,
                revision_id,
            } => (0, to_sql_integer(wiki_id)?, to_sql_integer(revision_id)?),
            IntegrityMetadataSubject::Page { wiki_id, page_id } => {
                (1, to_sql_integer(wiki_id)?, to_sql_integer(page_id)?)
            }
            IntegrityMetadataSubject::Checkpoint { checkpoint_id } => {
                (2, to_sql_integer(checkpoint_id)?, 0)
            }
            IntegrityMetadataSubject::SearchDocument { search_id } => {
                (3, to_sql_integer(search_id)?, 0)
            }
            IntegrityMetadataSubject::SearchFtsRow { row_id } => (4, row_id, 0),
            IntegrityMetadataSubject::Media { row_id, .. } => (5, row_id, 0),
            IntegrityMetadataSubject::PageMedia { row_id, .. } => (6, row_id, 0),
        };
        Ok(IntegrityMetadataCursor {
            category,
            first_key,
            second_key,
        })
    }
}

impl ObjectVerificationState {
    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "pending" => Ok(Self::Pending),
            "verified" => Ok(Self::Verified),
            "corrupt" => Ok(Self::Corrupt),
            _ => Err(StoreError::CorruptMetadata(
                "unknown object verification state",
            )),
        }
    }
}

/// The source-level operation represented by a durable synchronization run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncRunKind {
    /// Initial collection population.
    Bootstrap,
    /// Routine overlap-window update.
    Update,
    /// Explicit historical backfill.
    History,
    /// Long-gap source reconciliation.
    Reconciliation,
}

impl SyncRunKind {
    /// Returns the stable database and JSON representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Update => "update",
            Self::History => "history",
            Self::Reconciliation => "reconciliation",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "bootstrap" => Ok(Self::Bootstrap),
            "update" => Ok(Self::Update),
            "history" => Ok(Self::History),
            "reconciliation" => Ok(Self::Reconciliation),
            _ => Err(StoreError::CorruptMetadata("unknown sync run kind")),
        }
    }
}

/// Lifecycle state of a durable synchronization run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncRunState {
    /// Work may be claimed or resumed.
    Running,
    /// All jobs completed and the checkpoint was committed.
    Succeeded,
    /// Work was explicitly stopped without advancing the checkpoint.
    Cancelled,
}

impl SyncRunState {
    /// Returns the stable database and JSON representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(StoreError::CorruptMetadata("unknown sync run state")),
        }
    }
}

/// Lifecycle state of one idempotent synchronization job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncJobState {
    /// Durable and waiting to be claimed.
    Queued,
    /// Claimed by the current writer.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Failed and retained with structured error details.
    Failed,
}

impl SyncJobState {
    /// Returns the stable database and JSON representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            _ => Err(StoreError::CorruptMetadata("unknown sync job state")),
        }
    }
}

/// A persistent overlap checkpoint for one MediaWiki source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncCheckpoint {
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// Optional collection scope; absent for a whole-source run.
    pub collection_id: Option<CollectionId>,
    /// Highest source Unix timestamp fully committed locally.
    pub committed_through: u64,
    /// Amount subtracted from the checkpoint when planning the next update.
    pub overlap_seconds: u64,
    /// Opaque RecentChanges cursor associated with the committed boundary.
    pub recent_changes_cursor: Option<String>,
    /// Most recent completed long-gap reconciliation time.
    pub reconciled_at: Option<u64>,
    /// Run that last advanced this checkpoint.
    pub last_run_id: Option<u64>,
    /// Local time at which the checkpoint changed.
    pub updated_at: u64,
}

impl SyncCheckpoint {
    /// Returns the inclusive start of the next discovery window.
    #[must_use]
    pub const fn next_window_start(&self) -> u64 {
        self.committed_through.saturating_sub(self.overlap_seconds)
    }
}

/// One claimed durable work item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncJob {
    /// Local durable job identity.
    pub job_id: u64,
    /// Owning synchronization run.
    pub run_id: u64,
    /// Caller-defined idempotency key, unique within the run.
    pub key: String,
    /// Stable operation label interpreted by the synchronization engine.
    pub kind: String,
    /// Optional opaque subject such as a title or continuation token.
    pub subject: Option<String>,
    /// Current lifecycle state.
    pub state: SyncJobState,
    /// Number of times the job has been claimed.
    pub attempt_count: u32,
    /// Whether a failed job may be queued again when the run resumes.
    pub retryable: bool,
}

/// Aggregated status for one synchronization run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncRunStatus {
    /// Local durable run identity.
    pub run_id: u64,
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// Optional collection scope.
    pub collection_id: Option<CollectionId>,
    /// Source-level operation.
    pub kind: SyncRunKind,
    /// Current run state.
    pub state: SyncRunState,
    /// Inclusive source timestamp from which discovery began.
    pub window_start: u64,
    /// Source timestamp committed only after every job succeeds.
    pub checkpoint_candidate: u64,
    /// Immutable hash of the durable configuration at run start, absent for legacy runs.
    pub configuration_hash: Option<String>,
    /// Number of queued jobs.
    pub queued_jobs: u64,
    /// Number of claimed jobs.
    pub running_jobs: u64,
    /// Number of successful jobs.
    pub succeeded_jobs: u64,
    /// Number of failed jobs.
    pub failed_jobs: u64,
    /// Local creation time.
    pub created_at: u64,
    /// Local completion or cancellation time.
    pub finished_at: Option<u64>,
    /// Most recently recorded structured failure.
    pub latest_error: Option<SyncFailure>,
}

/// One structured synchronization failure retained for status and diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncFailure {
    /// Stable machine-readable failure category.
    pub code: String,
    /// Human-readable context.
    pub message: String,
    /// Whether resuming may safely retry the failed job.
    pub retryable: bool,
    /// Local time at which the failure was recorded.
    pub occurred_at: u64,
}

/// Result of starting a new run or recovering its existing durable work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartedSyncRun {
    /// Current aggregate status.
    pub status: SyncRunStatus,
    /// Whether an existing interrupted run was resumed.
    pub resumed: bool,
}

/// Identity and selection binding required to claim a current-page dump import.
///
/// `dump_digest` is the BLAKE3 identity of the authenticated dump-set index that
/// transitively commits the ordered artifact digests and lengths, encoded as `b3:`
/// followed by 64 lowercase hexadecimal digits. The caller must authenticate every
/// committed artifact before starting canonical import.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumpImportRequest<'a> {
    /// Running, collection-scoped bootstrap synchronization run.
    pub run_id: u64,
    /// Authenticated dump-set/index identity.
    pub dump_digest: &'a str,
    /// Exact total compressed length of the authenticated dump-set artifacts.
    pub dump_compressed_bytes: u64,
    /// Collection generation used to resolve the imported selection.
    pub collection_generation: u64,
    /// Source timestamp captured before dump import, used for race-window closure.
    pub bootstrap_started_at: u64,
}

/// Lifecycle of one durable current-page dump import.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DumpImportState {
    /// The current writer has claimed the import and may advance it.
    Running,
    /// Every artifact in the authenticated dump set was scanned successfully.
    Succeeded,
    /// Import stopped with a retained structured failure.
    Failed,
}

impl DumpImportState {
    /// Returns the stable lowercase SQLite and status representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            _ => Err(StoreError::CorruptMetadata("unknown dump import state")),
        }
    }
}

/// Durable status and exact resume identity for one current-page dump import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpImportStatus {
    /// Local durable import identity.
    pub import_id: u64,
    /// Owning bootstrap synchronization run.
    pub run_id: u64,
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// Collection whose resolved selection is being populated.
    pub collection_id: CollectionId,
    /// BLAKE3 digest of the authenticated dump-set index.
    pub dump_digest: String,
    /// Exact total compressed length of the authenticated dump-set artifacts.
    pub dump_compressed_bytes: u64,
    /// Collection generation to which the import is bound.
    pub collection_generation: u64,
    /// Immutable synchronization configuration hash captured by the owning run.
    pub configuration_hash: String,
    /// Source timestamp from immediately before dump import began.
    pub bootstrap_started_at: u64,
    /// Current lifecycle state.
    pub state: DumpImportState,
    /// Number of complete dump pages scanned; this is the sequential resume cursor.
    pub pages_scanned: u64,
    /// Number of distinct selected pages durably recorded for this import.
    pub imported_pages: u64,
    /// Sum of canonical source bytes for distinct selected pages.
    pub imported_canonical_bytes: u64,
    /// Number of times a writer has claimed this import.
    pub attempt_count: u32,
    /// Whether a failed import may be reclaimed.
    pub retryable: bool,
    /// Local creation time.
    pub created_at: u64,
    /// Most recent claim time.
    pub claimed_at: u64,
    /// Most recent durable progress or state-change time.
    pub updated_at: u64,
    /// Local completion/failure time.
    pub finished_at: Option<u64>,
    /// Most recently recorded failure, if the import failed.
    pub latest_error: Option<SyncFailure>,
}

/// Result of atomically claiming new or restartable dump-import work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartedDumpImport {
    /// Current durable import status.
    pub status: DumpImportStatus,
    /// Whether the exact durable identity existed before this claim.
    pub resumed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PackEncoding {
    Full,
    Delta,
}

impl PackEncoding {
    const fn tag(self) -> u8 {
        match self {
            Self::Full => PACK_ENCODING_FULL,
            Self::Delta => PACK_ENCODING_DELTA,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, StoreError> {
        match tag {
            PACK_ENCODING_FULL => Ok(Self::Full),
            PACK_ENCODING_DELTA => Ok(Self::Delta),
            _ => Err(StoreError::CorruptPack("unknown pack entry encoding")),
        }
    }

    const fn database_value(self) -> &'static str {
        match self {
            Self::Full => "pack-full",
            Self::Delta => "pack-delta",
        }
    }
}

#[derive(Debug)]
struct PreparedPackEntry {
    id: ObjectId,
    kind: ObjectKind,
    uncompressed_length: u64,
    encoding: PackEncoding,
    base_id: Option<ObjectId>,
    delta_depth: u16,
    payload: Vec<u8>,
    offset: u64,
    record_length: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PackAffinity {
    wiki_id: u64,
    page_id: u64,
}

#[derive(Debug)]
struct PackSource {
    id: ObjectId,
    kind: ObjectKind,
    bytes: Vec<u8>,
    affinity: Option<PackAffinity>,
    revision_order: Option<u64>,
    stable_order: u64,
}

impl PackSource {
    fn sort_key(&self) -> (u8, bool, u64, u64, u32, u64, u64, ObjectId) {
        let affinity = self.affinity.unwrap_or(PackAffinity {
            wiki_id: 0,
            page_id: 0,
        });
        (
            self.kind.identity_tag(),
            self.affinity.is_none(),
            affinity.wiki_id,
            affinity.page_id,
            object_size_class(self.bytes.len() as u64),
            self.revision_order.unwrap_or(0),
            self.stable_order,
            self.id,
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct PackIndexEntry {
    id: ObjectId,
    offset: u64,
    record_length: u64,
}

#[derive(Debug)]
struct DecodedPackEntry {
    id: ObjectId,
    kind: ObjectKind,
    uncompressed_length: u64,
    encoding: PackEncoding,
    base_id: Option<ObjectId>,
    delta_depth: u16,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct PackLocation {
    storage_kind: String,
    encoding: String,
    relative_path: String,
    compressed_length: i64,
    base_object_id: Option<String>,
    pack_id: Option<String>,
    pack_offset: Option<i64>,
    delta_depth: Option<i64>,
    pack_path: Option<String>,
    index_path: Option<String>,
    pack_checksum: Option<String>,
    index_checksum: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct MediaMetadataRow {
    file_title: String,
    original_url: String,
    description_url: String,
    author: String,
    attribution: String,
    license_name: String,
    license_url: Option<String>,
    width: i64,
    height: i64,
    mime_type: String,
}

#[derive(Debug)]
struct RevisionMediaRow {
    placement_index: i64,
    placement_kind: String,
    caption: Option<String>,
    alt_text: Option<String>,
    source_media_id: i64,
    source_sha1: String,
    file_title: String,
    original_url: String,
    description_url: String,
    author: String,
    attribution: String,
    license_name: String,
    license_url: Option<String>,
    width: i64,
    height: i64,
    mime_type: String,
    captured_at: i64,
    content_object_id: String,
}

#[derive(Debug)]
struct RecordedPack {
    pack_path: PathBuf,
    index_path: PathBuf,
    pack_checksum: [u8; 32],
    index_checksum: [u8; 32],
    generation: u64,
    object_count: u64,
}

#[derive(Debug)]
struct DumpImportRow {
    import_id: i64,
    run_id: i64,
    wiki_id: i64,
    collection_id: i64,
    dump_digest: String,
    dump_compressed_bytes: i64,
    collection_generation: i64,
    configuration_hash: String,
    bootstrap_started_at: i64,
    state: String,
    pages_scanned: i64,
    imported_pages: i64,
    imported_canonical_bytes: i64,
    attempt_count: i64,
    retryable: bool,
    error_code: Option<String>,
    error_message: Option<String>,
    created_at: i64,
    claimed_at: i64,
    updated_at: i64,
    finished_at: Option<i64>,
}

type DumpImportRunBinding = (i64, Option<i64>, String, String, i64, Option<String>);

/// One WikiSyncer library and its SQLite connection.
#[derive(Debug)]
pub struct Library {
    root: PathBuf,
    connection: Connection,
    config: StoreConfig,
    read_only: bool,
}

fn prepare_library_directories(root: &Path) -> io::Result<()> {
    for directory in [
        root.to_path_buf(),
        root.join("objects"),
        root.join("objects/loose"),
        root.join("objects/loose/b3"),
        root.join("objects/packs"),
        root.join(MANIFEST_DIRECTORY),
        root.join("tmp"),
    ] {
        create_private_dir_all(&directory)?;
    }
    Ok(())
}

fn create_private_dir_all(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn prepare_private_database_file(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "library database cannot be a symbolic link",
        ));
    }
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    drop(options.open(path)?);
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn restrict_sqlite_file_permissions(database_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    for suffix in ["", "-wal", "-shm"] {
        let mut path = database_path.as_os_str().to_os_string();
        path.push(suffix);
        let path = PathBuf::from(path);
        if path.exists() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    let _ = database_path;
    Ok(())
}

fn immutable_sqlite_uri(database_path: &Path) -> Result<String, StoreError> {
    let absolute = fs::canonicalize(database_path)?;
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt as _;
        absolute.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let path_text = absolute.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = path_text.as_bytes();

    let mut uri = String::from("file:");
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            write!(uri, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    Ok(uri)
}

impl Library {
    /// Opens or creates a library using default object bounds.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_config(root, StoreConfig::default())
    }

    /// Opens an existing library without changing its database or filesystem layout.
    ///
    /// The SQLite connection is both operating-system read-only and `query_only`.
    /// This path does not create directories or files, apply migrations, change file
    /// permissions, or alter persistent journal and synchronization settings.
    pub fn open_read_only(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        let database_path = root.join(DATABASE_NAME);
        let metadata = fs::symlink_metadata(&database_path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StoreError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "library database must be an existing regular file",
            )));
        }

        let database_uri = immutable_sqlite_uri(&database_path)?;
        let connection = Connection::open_with_flags(
            database_uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        connection.pragma_update(None, "query_only", true)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;

        Ok(Self {
            root,
            connection,
            config: StoreConfig::default(),
            read_only: true,
        })
    }

    /// Opens or creates a library, configures SQLite, and applies migrations.
    pub fn open_with_config(
        root: impl AsRef<Path>,
        config: StoreConfig,
    ) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        prepare_library_directories(&root)?;

        let database_path = root.join(DATABASE_NAME);
        prepare_private_database_file(&database_path)?;
        let connection = Connection::open(&database_path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&connection)?;
        restrict_sqlite_file_permissions(&database_path)?;

        Ok(Self {
            root,
            connection,
            config,
            read_only: false,
        })
    }

    fn ensure_writable(&self) -> Result<(), StoreError> {
        if self.read_only {
            Err(StoreError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Returns the library root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the SQLite database path for read/index services sharing this library.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.root.join(DATABASE_NAME)
    }

    /// Returns the durable library-wide network transfer policy.
    pub fn network_transfer_policy(&self) -> Result<NetworkTransferPolicy, StoreError> {
        read_network_transfer_policy(&self.connection)
    }

    /// Atomically replaces the durable library-wide network transfer policy.
    pub fn update_network_transfer_policy(
        &mut self,
        policy: NetworkTransferPolicy,
    ) -> Result<(), StoreError> {
        self.ensure_writable()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE network_transfer_policy
             SET max_concurrent_requests = ?1,
                 max_download_bytes_per_second = ?2,
                 avoid_metered_networks = ?3
             WHERE singleton = 1",
            params![
                i64::from(policy.max_concurrent_requests()),
                policy
                    .max_download_bytes_per_second()
                    .map(to_sql_integer)
                    .transpose()?,
                policy.avoid_metered_networks(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptMetadata(
                "network transfer policy row is missing",
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Registers a MediaWiki source, returning the existing identity on repetition.
    pub fn register_wiki(
        &mut self,
        api_endpoint: &str,
        language_code: &str,
    ) -> Result<WikiId, StoreError> {
        if api_endpoint.trim().is_empty() || language_code.trim().is_empty() {
            return Err(StoreError::InvalidConfig(
                "wiki endpoint and language code must be non-empty",
            ));
        }
        let now = unix_time()?;
        self.connection.execute(
            "INSERT OR IGNORE INTO wikis (api_endpoint, language_code, created_at)
             VALUES (?1, ?2, ?3)",
            params![api_endpoint, language_code, now],
        )?;
        let raw_id: i64 = self.connection.query_row(
            "SELECT wiki_id FROM wikis WHERE api_endpoint = ?1",
            [api_endpoint],
            |row| row.get(0),
        )?;
        sql_id(raw_id, "invalid wiki ID")
    }

    /// Looks up one configured source without exposing the SQLite connection.
    pub fn wiki(&self, wiki_id: WikiId) -> Result<Option<StoredWiki>, StoreError> {
        self.connection
            .query_row(
                "SELECT api_endpoint, language_code FROM wikis WHERE wiki_id = ?1",
                [to_sql_integer(wiki_id.get())?],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(api_endpoint, language_code)| {
                Ok(StoredWiki {
                    wiki_id,
                    api_endpoint,
                    language_code,
                })
            })
            .transpose()
    }

    /// Lists configured sources in stable identity order.
    pub fn wikis(&self) -> Result<Vec<StoredWiki>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT wiki_id, api_endpoint, language_code FROM wikis ORDER BY wiki_id")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(wiki_id, api_endpoint, language_code)| {
                Ok(StoredWiki {
                    wiki_id: sql_id(wiki_id, "invalid wiki ID")?,
                    api_endpoint,
                    language_code,
                })
            })
            .collect()
    }

    /// Removes an unused source registration without deleting captured evidence.
    ///
    /// A source is unused only when it has no collections, captured pages,
    /// synchronization runs or checkpoints, and no immutable manifest names it.
    /// The operation otherwise fails with [`StoreError::WikiInUse`].
    pub fn remove_wiki(&mut self, wiki_id: WikiId) -> Result<(), StoreError> {
        self.ensure_writable()?;
        let raw_wiki_id = to_sql_integer(wiki_id.get())?;
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM wikis WHERE wiki_id = ?1)",
            [raw_wiki_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::WikiNotFound(wiki_id));
        }

        let manifests = self
            .validated_manifest_chain()?
            .into_iter()
            .filter(|stored| stored.manifest.wiki_id == wiki_id)
            .count();
        let manifests = u64::try_from(manifests).map_err(|_| StoreError::ManifestLimitExceeded)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (collections, captured_pages, sync_runs, checkpoints): (i64, i64, i64, i64) =
            transaction.query_row(
                "SELECT
                    (SELECT COUNT(*) FROM collections WHERE wiki_id = ?1),
                    (SELECT COUNT(*) FROM pages WHERE wiki_id = ?1),
                    (SELECT COUNT(*) FROM sync_runs WHERE wiki_id = ?1),
                    (SELECT COUNT(*) FROM sync_checkpoints WHERE wiki_id = ?1)",
                [raw_wiki_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let collections = sql_u64(collections, "invalid source collection count")?;
        let captured_pages = sql_u64(captured_pages, "invalid source page count")?;
        let sync_runs = sql_u64(sync_runs, "invalid source sync-run count")?;
        let checkpoints = sql_u64(checkpoints, "invalid source checkpoint count")?;
        if collections != 0
            || captured_pages != 0
            || sync_runs != 0
            || checkpoints != 0
            || manifests != 0
        {
            return Err(StoreError::WikiInUse {
                wiki_id,
                collections,
                captured_pages,
                sync_runs,
                checkpoints,
                manifests,
            });
        }
        let changed = transaction.execute("DELETE FROM wikis WHERE wiki_id = ?1", [raw_wiki_id])?;
        if changed != 1 {
            return Err(StoreError::WikiNotFound(wiki_id));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Creates or reopens an explicit-title, current-and-future collection.
    pub fn create_explicit_collection(
        &mut self,
        wiki_id: WikiId,
        name: &str,
    ) -> Result<CollectionId, StoreError> {
        if name.trim().is_empty() {
            return Err(StoreError::InvalidConfig(
                "collection name must be non-empty",
            ));
        }
        let now = unix_time()?;
        self.connection.execute(
            "INSERT OR IGNORE INTO collections (
                wiki_id, name, rule_kind, history_policy, created_at
             ) VALUES (?1, ?2, 'explicit-titles', 'current-and-future', ?3)",
            params![to_sql_integer(wiki_id.get())?, name, now],
        )?;
        let (raw_id, status): (i64, String) = self.connection.query_row(
            "SELECT collection_id, status FROM collections WHERE wiki_id = ?1 AND name = ?2",
            params![to_sql_integer(wiki_id.get())?, name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let collection_id: CollectionId = sql_id(raw_id, "invalid collection ID")?;
        if stored_collection_status(&status)? == CollectionStatus::Tombstoned {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO collection_schedules (
                collection_id, cadence_kind, cadence_seconds, jitter_seconds,
                paused, next_run_at, last_started_at, updated_at
             ) VALUES (?1, 'manual', NULL, 0, 0, NULL, NULL, ?2)",
            params![to_sql_integer(collection_id.get())?, now],
        )?;
        self.connection.execute(
            "INSERT OR IGNORE INTO collection_configuration (
                collection_id, rule_kind, category_title, category_recursion_depth,
                history_kind, history_value, maximum_pages, maximum_bytes,
                removal_policy, updated_at
             ) VALUES (?1, 'explicit-titles', NULL, NULL,
                       'current-and-future', NULL, NULL, NULL,
                       'stop-tracking-retain-history', ?2)",
            params![to_sql_integer(collection_id.get())?, now],
        )?;
        Ok(collection_id)
    }

    /// Renames an active collection without changing its stable identity or evidence.
    pub fn rename_collection(
        &mut self,
        collection_id: CollectionId,
        name: &str,
    ) -> Result<(), StoreError> {
        self.ensure_writable()?;
        if name.trim().is_empty() {
            return Err(StoreError::InvalidConfig(
                "collection name must be non-empty",
            ));
        }
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        ensure_collection_active(&self.connection, collection_id, raw_collection_id)?;
        let changed = self.connection.execute(
            "UPDATE collections SET name = ?2, generation = generation + 1
             WHERE collection_id = ?1 AND status = 'active'",
            params![raw_collection_id, name],
        )?;
        if changed != 1 {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        Ok(())
    }

    /// Stops tracking a collection while retaining all canonical and audit history.
    ///
    /// The operation is idempotent. It atomically disables its schedule, cancels any
    /// unfinished run without advancing a checkpoint, and marks active membership as
    /// removed. Collection, configuration, resolved-member, checkpoint, sync-run and
    /// manifest scope identities remain intact; no canonical page, revision or object
    /// is deleted.
    pub fn tombstone_collection(&mut self, collection_id: CollectionId) -> Result<(), StoreError> {
        self.ensure_writable()?;
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status = collection_status(&transaction, collection_id, raw_collection_id)?;
        if status == CollectionStatus::Tombstoned {
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "UPDATE sync_runs
             SET state = 'cancelled', finished_at = ?2
             WHERE collection_id = ?1 AND state = 'running'",
            params![raw_collection_id, now],
        )?;
        transaction.execute(
            "UPDATE collection_schedules
             SET paused = 1, updated_at = ?2
             WHERE collection_id = ?1",
            params![raw_collection_id, now],
        )?;
        transaction.execute(
            "UPDATE collection_resolved_members
             SET membership_state = 'removed', removed_at = ?2
             WHERE collection_id = ?1 AND membership_state = 'active'",
            params![raw_collection_id, now],
        )?;
        transaction.execute(
            "DELETE FROM collection_pages WHERE collection_id = ?1",
            [raw_collection_id],
        )?;
        let changed = transaction.execute(
            "UPDATE collections
             SET status = 'tombstoned', tombstoned_at = ?2,
                 generation = generation + 1
             WHERE collection_id = ?1 AND status = 'active'",
            params![raw_collection_id, now],
        )?;
        if changed != 1 {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Computes a bounded deterministic preview of canonical payload referenced only
    /// by one tombstoned collection.
    ///
    /// This does not write the journal or remove metadata, object locations, or files.
    /// Pages shared with any other retained collection are excluded, as are logical
    /// objects referenced by any page outside the exclusive target-page closure.
    pub fn preview_collection_purge(
        &self,
        collection_id: CollectionId,
    ) -> Result<PurgePreview, StoreError> {
        let manifests = self.validated_manifest_chain()?;
        let (manifest_head, protected_manifest_objects) =
            purge_manifest_binding(&manifests, collection_id);
        compute_purge_preview(
            &self.connection,
            collection_id,
            manifest_head,
            &protected_manifest_objects,
        )
        .map(|(preview, _, _)| preview)
    }

    /// Durably authorizes the exact currently valid purge preview.
    ///
    /// Authorization re-runs the exclusive-reference closure in an immediate SQLite
    /// transaction and snapshots every selected logical object and affected pack.
    /// It remains non-destructive: later checkpoints perform verified replacement-pack
    /// activation, authenticated manifest publication, and restartable file cleanup.
    /// Retrying the exact authorization returns the existing unfinished receipt.
    pub fn authorize_collection_purge(
        &mut self,
        collection_id: CollectionId,
        authorization: PurgeAuthorization<'_>,
    ) -> Result<AuthorizedPurge, StoreError> {
        self.ensure_writable()?;
        if !authorization.payload_only_acknowledged
            || !authorization.backups_not_erased_acknowledged
        {
            return Err(StoreError::PurgeAcknowledgementsRequired);
        }
        authorization
            .preview_fingerprint
            .parse::<ManifestId>()
            .map_err(|_| StoreError::InvalidPurgeFingerprint)?;

        let manifests = self.validated_manifest_chain()?;
        let (manifest_head, protected_manifest_objects) =
            purge_manifest_binding(&manifests, collection_id);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current = compute_purge_preview(
            &transaction,
            collection_id,
            manifest_head,
            &protected_manifest_objects,
        )
        .map_err(|error| match error {
            StoreError::NoExclusivePurgePayload(id) if id == collection_id => {
                StoreError::StalePurgePreview(collection_id)
            }
            StoreError::PurgeObjectUnavailable(_) => StoreError::StalePurgePreview(collection_id),
            other => other,
        })?;

        if let Some(existing) = authorized_purge_for_collection(&transaction, collection_id)? {
            if existing.preview.collection_name == authorization.collection_name
                && existing.preview.fingerprint == authorization.preview_fingerprint
            {
                if current.0 != existing.preview {
                    return Err(StoreError::StalePurgePreview(collection_id));
                }
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(StoreError::PurgeAlreadyPending(collection_id));
        }

        let (preview, candidates, packs) = current;
        if preview.collection_name != authorization.collection_name
            || preview.fingerprint != authorization.preview_fingerprint
        {
            return Err(StoreError::StalePurgePreview(collection_id));
        }

        let now = unix_time()?;
        transaction.execute(
            "INSERT INTO purge_operations (
                collection_id, collection_name, collection_generation, tombstoned_at,
                manifest_head_sequence, manifest_head_id, preview_fingerprint,
                object_count, wikitext_object_count, media_object_count, logical_bytes,
                reclaimable_bytes, loose_object_count, affected_pack_count,
                whole_pack_count, mixed_pack_count, state,
                acknowledged_collection_name, acknowledged_preview_fingerprint,
                payload_only_acknowledged, backups_not_erased_acknowledged,
                created_at, authorized_at, updated_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, 'authorized', ?2, ?7, 1, 1, ?17, ?17, ?17
             )",
            params![
                to_sql_integer(collection_id.get())?,
                preview.collection_name,
                to_sql_integer(preview.collection_generation)?,
                to_sql_integer(preview.tombstoned_at)?,
                preview
                    .manifest_head_sequence
                    .map(to_sql_integer)
                    .transpose()?,
                preview.manifest_head_id.map(|id| id.to_string()),
                preview.fingerprint,
                to_sql_integer(preview.object_count)?,
                to_sql_integer(preview.wikitext_object_count)?,
                to_sql_integer(preview.media_object_count)?,
                to_sql_integer(preview.logical_bytes)?,
                to_sql_integer(preview.reclaimable_bytes)?,
                to_sql_integer(preview.loose_object_count)?,
                to_sql_integer(preview.affected_pack_count)?,
                to_sql_integer(preview.whole_pack_count)?,
                to_sql_integer(preview.mixed_pack_count)?,
                now,
            ],
        )?;
        let raw_purge_id = transaction.last_insert_rowid();
        let purge_id = sql_u64(raw_purge_id, "invalid purge ID")?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO purge_objects (
                    purge_id, object_id, object_kind, uncompressed_length
                 ) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for candidate in &candidates {
                insert.execute(params![
                    raw_purge_id,
                    candidate.id.to_string(),
                    candidate.kind.database_value(),
                    to_sql_integer(candidate.uncompressed_length)?,
                ])?;
            }
        }
        {
            let mut insert = transaction.prepare(
                "INSERT INTO purge_pack_work (
                    purge_id, old_pack_id, purged_object_count,
                    retained_object_count, replacement_pack_id, state
                 ) VALUES (?1, ?2, ?3, ?4, NULL, 'pending')",
            )?;
            for pack in &packs {
                insert.execute(params![
                    raw_purge_id,
                    pack.pack_id,
                    to_sql_integer(pack.purged_object_count)?,
                    to_sql_integer(pack.retained_object_count)?,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(AuthorizedPurge {
            purge_id,
            preview,
            authorized_at: sql_u64(now, "invalid purge authorization time")?,
        })
    }

    /// Returns a bounded page of the logical-object journal for an authorized purge.
    pub fn purge_objects_after(
        &self,
        purge_id: u64,
        after: Option<ObjectId>,
        limit: u32,
    ) -> Result<Vec<PurgeObject>, StoreError> {
        if !(1..=MAX_PURGE_OBJECT_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "purge object page size must be between 1 and 1,000",
            ));
        }
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM purge_operations WHERE purge_id = ?1)",
            [to_sql_integer(purge_id)?],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::PurgeNotFound(purge_id));
        }
        let after = after.map(|id| id.to_string()).unwrap_or_default();
        let mut statement = self.connection.prepare(
            "SELECT object_id, object_kind, uncompressed_length
             FROM purge_objects
             WHERE purge_id = ?1 AND object_id > ?2
             ORDER BY object_id LIMIT ?3",
        )?;
        let rows = statement
            .query_map(params![to_sql_integer(purge_id)?, after, limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, kind, length)| {
                Ok(PurgeObject {
                    object: StoredObject {
                        id: id
                            .parse()
                            .map_err(|_| StoreError::CorruptMetadata("invalid purge object ID"))?,
                        kind: ObjectKind::from_database(&kind)?,
                        uncompressed_length: sql_u64(length, "invalid purge object length")?,
                    },
                })
            })
            .collect()
    }

    /// Creates a new collection and atomically commits its complete initial policy.
    ///
    /// A duplicate source/name is rejected; validation or persistence failure leaves
    /// no draft collection or child configuration behind.
    pub fn create_collection(
        &mut self,
        wiki_id: WikiId,
        name: &str,
        rule: &CollectionRule,
        history_policy: HistoryPolicy,
        budget: CollectionBudget,
        removal_policy: CollectionRemovalPolicy,
    ) -> Result<CollectionId, StoreError> {
        self.create_collection_from_preview(
            wiki_id,
            name,
            CollectionPreviewCommit {
                rule,
                history_policy,
                budget,
                removal_policy,
                members: &[],
                missing_titles: &[],
                predicted_canonical_bytes: None,
            },
        )
        .map(|(collection_id, _membership)| collection_id)
    }

    /// Creates a collection and commits its complete preview in one transaction.
    ///
    /// Validation or budget failure leaves no draft collection, schedule, estimate,
    /// unresolved title, or membership row behind.
    pub fn create_collection_from_preview(
        &mut self,
        wiki_id: WikiId,
        name: &str,
        preview: CollectionPreviewCommit<'_>,
    ) -> Result<(CollectionId, MembershipCommit), StoreError> {
        self.create_collection_from_preview_with_image_policy(
            wiki_id,
            name,
            preview,
            ImagePolicy::None,
        )
    }

    /// Creates a collection and atomically commits its complete preview and image
    /// policy in one generation.
    ///
    /// This is the image-aware counterpart to [`Self::create_collection_from_preview`].
    /// The older method remains explicitly default-off for source compatibility.
    pub fn create_collection_from_preview_with_image_policy(
        &mut self,
        wiki_id: WikiId,
        name: &str,
        preview: CollectionPreviewCommit<'_>,
        image_policy: ImagePolicy,
    ) -> Result<(CollectionId, MembershipCommit), StoreError> {
        self.ensure_writable()?;
        validate_collection_name(name)?;
        validate_preview_commit(preview)?;
        let now = unix_time()?;
        let raw_wiki_id = to_sql_integer(wiki_id.get())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw_collection_id: i64 = transaction.query_row(
            "INSERT INTO collections (
                wiki_id, name, rule_kind, history_policy, created_at
             ) VALUES (?1, ?2, 'explicit-titles', 'current-and-future', ?3)
             RETURNING collection_id",
            params![raw_wiki_id, name, now],
            |row| row.get(0),
        )?;
        let collection_id = sql_id(raw_collection_id, "invalid collection ID")?;
        transaction.execute(
            "INSERT INTO collection_schedules (
                collection_id, cadence_kind, cadence_seconds, jitter_seconds,
                paused, next_run_at, last_started_at, updated_at
             ) VALUES (?1, 'manual', NULL, 0, 0, NULL, NULL, ?2)",
            params![raw_collection_id, now],
        )?;
        let membership = commit_preview_transaction(
            &transaction,
            raw_collection_id,
            raw_wiki_id,
            preview,
            image_policy,
            now,
        )?;
        transaction.commit()?;
        Ok((collection_id, membership))
    }

    /// Atomically replaces an active collection's name, policy, estimate and preview.
    ///
    /// `expected_generation` must match the generation read before preview began.
    /// Passing `None` for `name` retains the current name. Every other input is a
    /// complete replacement, including unresolved titles and resolved membership.
    /// A successful commit advances the generation exactly once.
    pub fn update_collection_from_preview(
        &mut self,
        collection_id: CollectionId,
        expected_generation: u64,
        name: Option<&str>,
        preview: CollectionPreviewCommit<'_>,
    ) -> Result<MembershipCommit, StoreError> {
        self.update_collection_from_preview_internal(
            collection_id,
            expected_generation,
            name,
            preview,
            None,
        )
    }

    /// Atomically replaces an active collection's complete preview and image policy.
    ///
    /// The expected generation is checked once and a successful transaction advances
    /// it exactly once. The older [`Self::update_collection_from_preview`] preserves
    /// the current image policy for source compatibility.
    pub fn update_collection_from_preview_with_image_policy(
        &mut self,
        collection_id: CollectionId,
        expected_generation: u64,
        name: Option<&str>,
        preview: CollectionPreviewCommit<'_>,
        image_policy: ImagePolicy,
    ) -> Result<MembershipCommit, StoreError> {
        self.update_collection_from_preview_internal(
            collection_id,
            expected_generation,
            name,
            preview,
            Some(image_policy),
        )
    }

    fn update_collection_from_preview_internal(
        &mut self,
        collection_id: CollectionId,
        expected_generation: u64,
        name: Option<&str>,
        preview: CollectionPreviewCommit<'_>,
        image_policy: Option<ImagePolicy>,
    ) -> Result<MembershipCommit, StoreError> {
        self.ensure_writable()?;
        if let Some(name) = name {
            validate_collection_name(name)?;
        }
        validate_preview_commit(preview)?;
        let now = unix_time()?;
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_collection_active(&transaction, collection_id, raw_collection_id)?;
        let raw_wiki_id: i64 = transaction.query_row(
            "SELECT wiki_id FROM collections WHERE collection_id = ?1",
            [raw_collection_id],
            |row| row.get(0),
        )?;
        let image_policy = match image_policy {
            Some(image_policy) => image_policy,
            None => stored_collection_image_policy(&transaction, collection_id, raw_collection_id)?,
        };
        let changed = transaction.execute(
            "UPDATE collections
             SET name = COALESCE(?3, name), generation = generation + 1
             WHERE collection_id = ?1 AND generation = ?2 AND status = 'active'",
            params![
                raw_collection_id,
                to_sql_integer(expected_generation)?,
                name
            ],
        )?;
        if changed != 1 {
            let actual: i64 = transaction.query_row(
                "SELECT generation FROM collections WHERE collection_id = ?1",
                [raw_collection_id],
                |row| row.get(0),
            )?;
            return Err(StoreError::StaleCollectionGeneration {
                collection_id,
                expected: expected_generation,
                actual: sql_u64(actual, "invalid collection generation")?,
            });
        }
        let membership = commit_preview_transaction(
            &transaction,
            raw_collection_id,
            raw_wiki_id,
            preview,
            image_policy,
            now,
        )?;
        transaction.commit()?;
        Ok(membership)
    }

    /// Atomically replaces a collection rule and its persisted policy fields.
    ///
    /// This commits only configuration. Potentially large resolved membership is
    /// committed separately with [`Self::commit_resolved_membership`] after preview.
    pub fn set_collection_configuration(
        &mut self,
        collection_id: CollectionId,
        rule: &CollectionRule,
        history_policy: HistoryPolicy,
        budget: CollectionBudget,
        removal_policy: CollectionRemovalPolicy,
    ) -> Result<(), StoreError> {
        let now = unix_time()?;
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let (rule_kind, category_title, category_depth) = collection_rule_values(rule);
        let (history_kind, history_value) = history_policy_values(history_policy)?;
        let maximum_pages = budget
            .maximum_pages()
            .map(|value| to_sql_integer(value.get()))
            .transpose()?;
        let maximum_bytes = budget
            .maximum_bytes()
            .map(|value| to_sql_integer(value.get()))
            .transpose()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_collection_active(&transaction, collection_id, raw_collection_id)?;
        transaction.execute(
            "INSERT INTO collection_configuration (
                collection_id, rule_kind, category_title, category_recursion_depth,
                history_kind, history_value, maximum_pages, maximum_bytes,
                removal_policy, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(collection_id) DO UPDATE SET
                rule_kind = excluded.rule_kind,
                category_title = excluded.category_title,
                category_recursion_depth = excluded.category_recursion_depth,
                history_kind = excluded.history_kind,
                history_value = excluded.history_value,
                maximum_pages = excluded.maximum_pages,
                maximum_bytes = excluded.maximum_bytes,
                removal_policy = excluded.removal_policy,
                updated_at = excluded.updated_at",
            params![
                raw_collection_id,
                rule_kind,
                category_title,
                category_depth,
                history_kind,
                history_value,
                maximum_pages,
                maximum_bytes,
                removal_policy_value(removal_policy),
                now,
            ],
        )?;
        transaction.execute(
            "DELETE FROM collection_rule_titles WHERE collection_id = ?1",
            [raw_collection_id],
        )?;
        if let Some(titles) = rule.titles() {
            for title in titles.iter() {
                transaction.execute(
                    "INSERT INTO collection_rule_titles (collection_id, title)
                     VALUES (?1, ?2)",
                    params![raw_collection_id, title.as_str()],
                )?;
            }
        }
        let changed = transaction.execute(
            "UPDATE collections SET generation = generation + 1
             WHERE collection_id = ?1 AND status = 'active'",
            [raw_collection_id],
        )?;
        if changed != 1 {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Reads a committed configuration; an old empty draft returns `None`.
    pub fn collection_configuration(
        &self,
        collection_id: CollectionId,
    ) -> Result<Option<StoredCollectionConfiguration>, StoreError> {
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let row = self
            .connection
            .query_row(
                "SELECT collections.wiki_id, collections.name, collections.generation,
                        collections.status,
                        config.rule_kind, config.category_title,
                        config.category_recursion_depth, config.history_kind,
                        config.history_value, config.maximum_pages,
                        config.maximum_bytes, config.removal_policy,
                        config.image_policy, config.thumbnail_max_edge_pixels,
                        config.thumbnail_max_images_per_revision,
                        config.thumbnail_max_bytes_per_image
                 FROM collections
                 JOIN collection_configuration AS config USING (collection_id)
                 WHERE collections.collection_id = ?1",
                [raw_collection_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, String>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, Option<i64>>(13)?,
                        row.get::<_, Option<i64>>(14)?,
                        row.get::<_, Option<i64>>(15)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            raw_wiki_id,
            name,
            generation,
            status,
            rule_kind,
            category_title,
            category_depth,
            history_kind,
            history_value,
            maximum_pages,
            maximum_bytes,
            removal_policy,
            image_policy,
            thumbnail_max_edge_pixels,
            thumbnail_max_images_per_revision,
            thumbnail_max_bytes_per_image,
        )) = row
        else {
            return Ok(None);
        };
        let rule = self.stored_collection_rule(
            raw_collection_id,
            &rule_kind,
            category_title,
            category_depth,
        )?;
        let Some(rule) = rule else {
            return Ok(None);
        };
        Ok(Some(StoredCollectionConfiguration {
            collection_id,
            wiki_id: sql_id(raw_wiki_id, "invalid wiki ID")?,
            name,
            generation: sql_u64(generation, "invalid collection generation")?,
            status: stored_collection_status(&status)?,
            rule,
            history_policy: stored_history_policy(&history_kind, history_value)?,
            budget: stored_collection_budget(maximum_pages, maximum_bytes)?,
            removal_policy: stored_removal_policy(&removal_policy)?,
            image_policy: stored_image_policy(
                &image_policy,
                thumbnail_max_edge_pixels,
                thumbnail_max_images_per_revision,
                thumbnail_max_bytes_per_image,
            )?,
        }))
    }

    /// Atomically replaces an active collection's optional image policy.
    ///
    /// Existing captured media is retained when the policy is disabled or reduced.
    /// The collection generation advances so stale administrative previews cannot
    /// silently overwrite this configuration change.
    pub fn set_collection_image_policy(
        &mut self,
        collection_id: CollectionId,
        image_policy: ImagePolicy,
    ) -> Result<(), StoreError> {
        self.ensure_writable()?;
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let (kind, maximum_edge, maximum_images, maximum_bytes) = image_policy_values(image_policy);
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_collection_active(&transaction, collection_id, raw_collection_id)?;
        let configured = transaction.execute(
            "UPDATE collection_configuration
             SET image_policy = ?2, thumbnail_max_edge_pixels = ?3,
                 thumbnail_max_images_per_revision = ?4,
                 thumbnail_max_bytes_per_image = ?5, updated_at = ?6
             WHERE collection_id = ?1",
            params![
                raw_collection_id,
                kind,
                maximum_edge,
                maximum_images,
                maximum_bytes,
                now,
            ],
        )?;
        if configured != 1 {
            return Err(StoreError::CollectionNotConfigured(collection_id));
        }
        let changed = transaction.execute(
            "UPDATE collections SET generation = generation + 1
             WHERE collection_id = ?1 AND status = 'active'",
            [raw_collection_id],
        )?;
        if changed != 1 {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        transaction.commit()?;
        Ok(())
    }

    fn stored_collection_rule(
        &self,
        raw_collection_id: i64,
        kind: &str,
        category_title: Option<String>,
        category_depth: Option<i64>,
    ) -> Result<Option<CollectionRule>, StoreError> {
        match kind {
            "explicit-titles" | "title-list" => {
                let mut statement = self.connection.prepare(
                    "SELECT title FROM collection_rule_titles
                     WHERE collection_id = ?1 ORDER BY title",
                )?;
                let raw_titles = statement
                    .query_map([raw_collection_id], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                if raw_titles.is_empty() {
                    return Ok(None);
                }
                let titles = raw_titles
                    .into_iter()
                    .map(|title| {
                        PageTitle::new(title).map_err(|_| {
                            StoreError::CorruptMetadata("invalid configured page title")
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let selection = TitleSelection::new(titles)
                    .map_err(|_| StoreError::CorruptMetadata("empty configured title rule"))?;
                Ok(Some(if kind == "explicit-titles" {
                    CollectionRule::ExplicitTitles(selection)
                } else {
                    CollectionRule::TitleList(selection)
                }))
            }
            "category" => {
                let title = category_title
                    .ok_or(StoreError::CorruptMetadata("category rule lacks title"))?;
                let depth = category_depth
                    .ok_or(StoreError::CorruptMetadata("category rule lacks depth"))?;
                Ok(Some(CollectionRule::Category {
                    title: PageTitle::new(title)
                        .map_err(|_| StoreError::CorruptMetadata("invalid category title"))?,
                    recursion_depth: u16::try_from(depth).map_err(|_| {
                        StoreError::CorruptMetadata("invalid category recursion depth")
                    })?,
                }))
            }
            _ => Err(StoreError::CorruptMetadata("unknown collection rule kind")),
        }
    }

    /// Records a title that MediaWiki currently reports as missing.
    pub fn record_missing_title(
        &mut self,
        collection_id: CollectionId,
        title: &PageTitle,
        namespace: i32,
    ) -> Result<(), StoreError> {
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        ensure_collection_active(&self.connection, collection_id, raw_collection_id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO unresolved_titles (
                collection_id, title, namespace, last_observed_at
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(collection_id, title) DO UPDATE SET
                namespace = excluded.namespace,
                last_observed_at = excluded.last_observed_at",
            params![raw_collection_id, title.as_str(), namespace, unix_time()?,],
        )?;
        let changed = transaction.execute(
            "UPDATE collections SET generation = generation + 1
             WHERE collection_id = ?1 AND status = 'active'",
            [raw_collection_id],
        )?;
        if changed != 1 {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Lists the currently unresolved explicit titles for a collection.
    pub fn unresolved_titles(
        &self,
        collection_id: CollectionId,
    ) -> Result<Vec<PageTitle>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT title FROM unresolved_titles
             WHERE collection_id = ?1 ORDER BY title",
        )?;
        let raw_titles = statement
            .query_map([to_sql_integer(collection_id.get())?], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raw_titles
            .into_iter()
            .map(|title| {
                PageTitle::new(title)
                    .map_err(|_| StoreError::CorruptMetadata("invalid unresolved page title"))
            })
            .collect()
    }

    /// Atomically commits a fully resolved rule preview as active membership.
    ///
    /// Members need not have captured revisions yet. Stable page IDs, titles,
    /// namespaces, and inclusion reasons become durable together; the prior active
    /// result remains untouched if validation or the transaction fails.
    pub fn commit_resolved_membership(
        &mut self,
        collection_id: CollectionId,
        members: &[ResolvedCollectionMember],
    ) -> Result<MembershipCommit, StoreError> {
        let configuration = self
            .collection_configuration(collection_id)?
            .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
        if configuration.status == CollectionStatus::Tombstoned {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        let active_members = u64::try_from(members.len())
            .map_err(|_| StoreError::InvalidConfig("collection member count is too large"))?;
        if configuration
            .budget
            .maximum_pages()
            .is_some_and(|maximum| active_members > maximum.get())
        {
            return Err(StoreError::CollectionBudgetExceeded {
                resource: "pages",
                limit: configuration
                    .budget
                    .maximum_pages()
                    .expect("checked maximum")
                    .get(),
                estimated: active_members,
            });
        }
        let mut unique_page_ids = HashSet::with_capacity(members.len());
        for member in members {
            if !unique_page_ids.insert(member.page_id) {
                return Err(StoreError::InvalidConfig(
                    "resolved membership contains a duplicate page ID",
                ));
            }
            validate_inclusion_reason(&configuration.rule, member)?;
        }

        let now = unix_time()?;
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let raw_wiki_id = to_sql_integer(configuration.wiki_id.get())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if configuration.removal_policy == CollectionRemovalPolicy::StopTrackingRetainHistory {
            transaction.execute(
                "UPDATE collection_resolved_members
                 SET membership_state = 'removed', removed_at = ?2
                 WHERE collection_id = ?1 AND membership_state = 'active'",
                params![raw_collection_id, now],
            )?;
            transaction.execute(
                "DELETE FROM collection_pages
                 WHERE collection_id = ?1
                   AND page_id IN (
                       SELECT page_id FROM collection_resolved_members
                       WHERE collection_id = ?1 AND membership_state = 'removed'
                   )",
                [raw_collection_id],
            )?;
        }

        for member in members {
            let (kind, inclusion_title, inclusion_depth) =
                inclusion_reason_values(&member.inclusion_reason);
            let raw_page_id = to_sql_integer(member.page_id.get())?;
            transaction.execute(
                "INSERT INTO collection_resolved_members (
                    collection_id, wiki_id, page_id, namespace, title,
                    inclusion_kind, inclusion_title, inclusion_depth,
                    membership_state, first_resolved_at, last_resolved_at, removed_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                           'active', ?9, ?9, NULL)
                 ON CONFLICT(collection_id, page_id) DO UPDATE SET
                    wiki_id = excluded.wiki_id,
                    namespace = excluded.namespace,
                    title = excluded.title,
                    inclusion_kind = excluded.inclusion_kind,
                    inclusion_title = excluded.inclusion_title,
                    inclusion_depth = excluded.inclusion_depth,
                    membership_state = 'active',
                    last_resolved_at = excluded.last_resolved_at,
                    removed_at = NULL",
                params![
                    raw_collection_id,
                    raw_wiki_id,
                    raw_page_id,
                    member.namespace,
                    member.title.as_str(),
                    kind,
                    inclusion_title,
                    inclusion_depth,
                    now,
                ],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO collection_pages (
                    collection_id, wiki_id, page_id, inclusion_reason, added_at
                 )
                 SELECT ?1, ?2, ?3, 'explicit-title', ?4
                 WHERE EXISTS (
                    SELECT 1 FROM pages WHERE wiki_id = ?2 AND page_id = ?3
                 )",
                params![raw_collection_id, raw_wiki_id, raw_page_id, now],
            )?;
        }
        let raw_active_members: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM collection_resolved_members
             WHERE collection_id = ?1 AND membership_state = 'active'",
            [raw_collection_id],
            |row| row.get(0),
        )?;
        let active_members = sql_u64(raw_active_members, "invalid active member count")?;
        if configuration
            .budget
            .maximum_pages()
            .is_some_and(|maximum| active_members > maximum.get())
        {
            return Err(StoreError::CollectionBudgetExceeded {
                resource: "pages",
                limit: configuration
                    .budget
                    .maximum_pages()
                    .expect("checked maximum")
                    .get(),
                estimated: active_members,
            });
        }
        let raw_removed_members: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM collection_resolved_members
             WHERE collection_id = ?1 AND membership_state = 'removed'
               AND removed_at = ?2",
            params![raw_collection_id, now],
            |row| row.get(0),
        )?;
        let changed = transaction.execute(
            "UPDATE collections SET generation = generation + 1
             WHERE collection_id = ?1 AND status = 'active'",
            [raw_collection_id],
        )?;
        if changed != 1 {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        transaction.commit()?;
        Ok(MembershipCommit {
            active_members,
            removed_members: sql_u64(raw_removed_members, "invalid removed member count")?,
        })
    }

    /// Lists active resolved members in stable page-ID order, including uncaptured pages.
    pub fn resolved_collection_members(
        &self,
        collection_id: CollectionId,
    ) -> Result<Vec<ResolvedCollectionMember>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT page_id, namespace, title, inclusion_kind,
                    inclusion_title, inclusion_depth
             FROM collection_resolved_members
             WHERE collection_id = ?1 AND membership_state = 'active'
             ORDER BY page_id",
        )?;
        let rows = statement
            .query_map([to_sql_integer(collection_id.get())?], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(page_id, namespace, title, kind, inclusion_title, depth)| {
                    Ok(ResolvedCollectionMember {
                        page_id: sql_id(page_id, "invalid resolved page ID")?,
                        namespace,
                        title: PageTitle::new(title).map_err(|_| {
                            StoreError::CorruptMetadata("invalid resolved page title")
                        })?,
                        inclusion_reason: stored_inclusion_reason(&kind, inclusion_title, depth)?,
                    })
                },
            )
            .collect()
    }

    /// Records a resolver's bounded pre-capture page/byte prediction.
    pub fn record_collection_estimate(
        &mut self,
        collection_id: CollectionId,
        resolved_page_count: u64,
        predicted_canonical_bytes: Option<u64>,
    ) -> Result<CollectionEstimate, StoreError> {
        let configuration = self
            .collection_configuration(collection_id)?
            .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
        if configuration.status == CollectionStatus::Tombstoned {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        if !configuration.budget.permits(
            resolved_page_count,
            predicted_canonical_bytes.unwrap_or_default(),
        ) {
            let (resource, limit, estimated) = if configuration
                .budget
                .maximum_pages()
                .is_some_and(|limit| resolved_page_count > limit.get())
            {
                (
                    "pages",
                    configuration
                        .budget
                        .maximum_pages()
                        .expect("checked page limit")
                        .get(),
                    resolved_page_count,
                )
            } else {
                (
                    "bytes",
                    configuration
                        .budget
                        .maximum_bytes()
                        .expect("byte limit must be exceeded")
                        .get(),
                    predicted_canonical_bytes.unwrap_or_default(),
                )
            };
            return Err(StoreError::CollectionBudgetExceeded {
                resource,
                limit,
                estimated,
            });
        }
        self.connection.execute(
            "INSERT INTO collection_estimates (
                collection_id, resolved_page_count, predicted_canonical_bytes, estimated_at
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                to_sql_integer(collection_id.get())?,
                to_sql_integer(resolved_page_count)?,
                predicted_canonical_bytes.map(to_sql_integer).transpose()?,
                unix_time()?,
            ],
        )?;
        self.collection_estimate(collection_id)
    }

    /// Computes current captured usage and combines it with the latest prediction.
    pub fn collection_estimate(
        &self,
        collection_id: CollectionId,
    ) -> Result<CollectionEstimate, StoreError> {
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM collections WHERE collection_id = ?1)",
            [raw_collection_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::CollectionNotFound(collection_id));
        }
        let raw_page_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM collection_resolved_members
             WHERE collection_id = ?1 AND membership_state = 'active'",
            [raw_collection_id],
            |row| row.get(0),
        )?;
        let raw_current_bytes: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(uncompressed_length), 0)
             FROM content_objects
             WHERE object_id IN (
                SELECT DISTINCT revisions.content_object_id
                FROM collection_resolved_members AS members
                JOIN revisions
                  ON revisions.wiki_id = members.wiki_id
                 AND revisions.page_id = members.page_id
                WHERE members.collection_id = ?1
                  AND members.membership_state = 'active'
                UNION
                SELECT placements.content_object_id
                FROM collection_resolved_members AS members
                JOIN revisions
                  ON revisions.wiki_id = members.wiki_id
                 AND revisions.page_id = members.page_id
                JOIN page_media AS placements
                  ON placements.wiki_id = revisions.wiki_id
                 AND placements.revision_id = revisions.revision_id
                WHERE members.collection_id = ?1
                  AND members.membership_state = 'active'
             )",
            [raw_collection_id],
            |row| row.get(0),
        )?;
        let latest_prediction = self
            .connection
            .query_row(
                "SELECT predicted_canonical_bytes, estimated_at
                 FROM collection_estimates WHERE collection_id = ?1
                 ORDER BY estimated_at DESC, estimate_id DESC LIMIT 1",
                [raw_collection_id],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(CollectionEstimate {
            resolved_page_count: sql_u64(raw_page_count, "negative collection page count")?,
            current_canonical_bytes: sql_u64(
                raw_current_bytes,
                "negative collection canonical byte count",
            )?,
            predicted_canonical_bytes: latest_prediction
                .as_ref()
                .and_then(|(bytes, _)| *bytes)
                .map(|bytes| sql_u64(bytes, "negative predicted byte count"))
                .transpose()?,
            predicted_at: latest_prediction
                .map(|(_, timestamp)| sql_u64(timestamp, "negative estimate timestamp"))
                .transpose()?,
        })
    }

    /// Creates or replaces the durable schedule for a collection.
    ///
    /// Recurring cadences require a persisted next-run instant so a daemon can
    /// recover after restart or sleep. Manual cadence requires no next run or jitter.
    pub fn set_collection_schedule(
        &mut self,
        collection_id: CollectionId,
        cadence: ScheduleCadence,
        jitter_seconds: u32,
        paused: bool,
        next_run_at: Option<u64>,
    ) -> Result<CollectionSchedule, StoreError> {
        validate_schedule_configuration(cadence, jitter_seconds, next_run_at)?;
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let raw_next_run_at = next_run_at.map(to_sql_integer).transpose()?;
        let (cadence_kind, cadence_seconds) = schedule_cadence_values(cadence);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_collection_active(&transaction, collection_id, raw_collection_id)?;
        transaction.execute(
            "INSERT INTO collection_schedules (
                collection_id, cadence_kind, cadence_seconds, jitter_seconds,
                paused, next_run_at, last_started_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)
             ON CONFLICT(collection_id) DO UPDATE SET
                cadence_kind = excluded.cadence_kind,
                cadence_seconds = excluded.cadence_seconds,
                jitter_seconds = excluded.jitter_seconds,
                paused = excluded.paused,
                next_run_at = excluded.next_run_at,
                updated_at = excluded.updated_at",
            params![
                raw_collection_id,
                cadence_kind,
                cadence_seconds,
                jitter_seconds,
                paused,
                raw_next_run_at,
                unix_time()?,
            ],
        )?;
        transaction.commit()?;
        self.collection_schedule(collection_id)?
            .ok_or(StoreError::CorruptMetadata(
                "new collection schedule was not found",
            ))
    }

    /// Reads the durable schedule for one collection.
    pub fn collection_schedule(
        &self,
        collection_id: CollectionId,
    ) -> Result<Option<CollectionSchedule>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT collection_id, cadence_kind, cadence_seconds, jitter_seconds,
                        paused, next_run_at, last_started_at
                 FROM collection_schedules WHERE collection_id = ?1",
                [to_sql_integer(collection_id.get())?],
                schedule_row,
            )
            .optional()?;
        row.map(stored_schedule).transpose()
    }

    /// Lists all configured collection schedules in stable collection order.
    pub fn schedules(&self) -> Result<Vec<CollectionSchedule>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT collection_id, cadence_kind, cadence_seconds, jitter_seconds,
                    paused, next_run_at, last_started_at
             FROM collection_schedules ORDER BY collection_id",
        )?;
        let rows = statement
            .query_map([], schedule_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(stored_schedule).collect()
    }

    /// Lists a bounded page of unpaused schedules due at or before `now`.
    pub fn due_schedules(
        &self,
        now: u64,
        limit: u32,
    ) -> Result<Vec<CollectionSchedule>, StoreError> {
        if limit == 0 || limit > MAX_DUE_SCHEDULES {
            return Err(StoreError::InvalidConfig(
                "due schedule limit must be between 1 and 10,000",
            ));
        }
        let mut statement = self.connection.prepare(
            "SELECT collection_id, cadence_kind, cadence_seconds, jitter_seconds,
                    paused, next_run_at, last_started_at
             FROM collection_schedules AS schedules
             WHERE paused = 0 AND cadence_kind != 'manual' AND next_run_at <= ?1
               AND EXISTS (
                    SELECT 1 FROM collections
                    WHERE collections.collection_id = schedules.collection_id
                      AND collections.status = 'active'
               )
             ORDER BY next_run_at, collection_id LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![to_sql_integer(now)?, limit], schedule_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(stored_schedule).collect()
    }

    /// Atomically claims one due occurrence and advances its durable next run.
    ///
    /// `expected_next_run_at` is a compare-and-swap token obtained from
    /// [`Self::due_schedules`]. Exactly one caller can advance that occurrence. The
    /// replacement must be in the future relative to `started_at`, which lets a
    /// restarted daemon skip missed occurrences without immediately duplicating a
    /// start already durably claimed before a crash.
    pub fn claim_due_schedule(
        &mut self,
        collection_id: CollectionId,
        expected_next_run_at: u64,
        started_at: u64,
        next_run_at: u64,
    ) -> Result<Option<CollectionSchedule>, StoreError> {
        if next_run_at <= started_at {
            return Err(StoreError::InvalidConfig(
                "advanced schedule time must be later than its claim time",
            ));
        }
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE collection_schedules
             SET last_started_at = ?1, next_run_at = ?2, updated_at = ?1
             WHERE collection_id = ?3 AND paused = 0
               AND cadence_kind != 'manual'
               AND next_run_at = ?4 AND next_run_at <= ?1
               AND EXISTS (
                    SELECT 1 FROM collections
                    WHERE collections.collection_id = collection_schedules.collection_id
                      AND collections.status = 'active'
               )",
            params![
                to_sql_integer(started_at)?,
                to_sql_integer(next_run_at)?,
                raw_collection_id,
                to_sql_integer(expected_next_run_at)?,
            ],
        )?;
        let claimed = if changed == 1 {
            let row = transaction.query_row(
                "SELECT collection_id, cadence_kind, cadence_seconds, jitter_seconds,
                        paused, next_run_at, last_started_at
                 FROM collection_schedules WHERE collection_id = ?1",
                [raw_collection_id],
                schedule_row,
            )?;
            Some(stored_schedule(row)?)
        } else {
            None
        };
        transaction.commit()?;
        Ok(claimed)
    }

    /// Starts a synchronization run or resumes matching unfinished work.
    ///
    /// A new update starts at the persisted checkpoint minus its overlap. Recovering
    /// an interrupted run retains its original window and candidate, requeues jobs
    /// that were running at process exit, and requeues retryable failures. Completed
    /// jobs are never repeated.
    pub fn start_or_resume_sync_run(
        &mut self,
        wiki_id: WikiId,
        collection_id: Option<CollectionId>,
        kind: SyncRunKind,
        checkpoint_candidate: u64,
    ) -> Result<StartedSyncRun, StoreError> {
        let now = unix_time()?;
        let raw_wiki_id = to_sql_integer(wiki_id.get())?;
        let raw_collection_id = collection_id
            .map(|id| to_sql_integer(id.get()))
            .transpose()?;
        let raw_candidate = to_sql_integer(checkpoint_candidate)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let wiki_exists: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM wikis WHERE wiki_id = ?1)",
            [raw_wiki_id],
            |row| row.get(0),
        )?;
        if !wiki_exists {
            return Err(StoreError::WikiNotFound(wiki_id));
        }
        if let Some(raw_collection_id) = raw_collection_id {
            let collection: Option<(i64, String)> = transaction
                .query_row(
                    "SELECT wiki_id, status FROM collections WHERE collection_id = ?1",
                    [raw_collection_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((collection_wiki_id, status)) = collection else {
                return Err(StoreError::CollectionWikiMismatch);
            };
            if collection_wiki_id != raw_wiki_id {
                return Err(StoreError::CollectionWikiMismatch);
            }
            let collection_id = collection_id.expect("raw collection ID came from this value");
            if stored_collection_status(&status)? == CollectionStatus::Tombstoned {
                return Err(StoreError::CollectionTombstoned(collection_id));
            }
        }
        let configuration_hash =
            manifest_configuration_hash_for(&transaction, wiki_id, collection_id)?;

        transaction.execute(
            "INSERT OR IGNORE INTO sync_checkpoints (
                wiki_id, collection_id, committed_through, overlap_seconds, updated_at
             ) VALUES (?1, ?2, 0, ?3, ?4)",
            params![
                raw_wiki_id,
                raw_collection_id,
                to_sql_integer(DEFAULT_SYNC_OVERLAP_SECONDS)?,
                now
            ],
        )?;
        let existing_run: Option<(i64, String, Option<String>)> = transaction
            .query_row(
                "SELECT run_id, run_kind, configuration_hash FROM sync_runs
                 WHERE wiki_id = ?1 AND collection_id IS ?2
                   AND state = 'running'",
                params![raw_wiki_id, raw_collection_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let (raw_run_id, resumed) =
            if let Some((run_id, existing_kind, existing_configuration_hash)) = existing_run {
                if existing_kind != kind.as_str() {
                    return Err(StoreError::SyncScopeBusy {
                        run_id: sql_u64(run_id, "invalid sync run ID")?,
                        kind: existing_kind,
                    });
                }
                if existing_configuration_hash.is_none() {
                    transaction.execute(
                        "UPDATE sync_runs SET configuration_hash = ?2 WHERE run_id = ?1",
                        params![run_id, configuration_hash],
                    )?;
                }
                transaction.execute(
                    "UPDATE sync_jobs
                 SET state = 'queued', started_at = NULL, finished_at = NULL
                 WHERE run_id = ?1
                   AND (state = 'running' OR (state = 'failed' AND retryable = 1))",
                    [run_id],
                )?;
                (run_id, true)
            } else {
                let (committed_through, overlap_seconds): (i64, i64) = transaction.query_row(
                    "SELECT committed_through, overlap_seconds FROM sync_checkpoints
                 WHERE wiki_id = ?1 AND collection_id IS ?2",
                    params![raw_wiki_id, raw_collection_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let window_start = committed_through.saturating_sub(overlap_seconds).max(0);
                if raw_candidate < committed_through {
                    return Err(StoreError::InvalidCheckpointCandidate {
                        committed_through: sql_u64(committed_through, "invalid sync checkpoint")?,
                        candidate: checkpoint_candidate,
                    });
                }
                transaction.execute(
                    "INSERT INTO sync_runs (
                    wiki_id, collection_id, run_kind, state, window_start,
                    checkpoint_candidate, configuration_hash, created_at, started_at
                 ) VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6, ?7, ?7)",
                    params![
                        raw_wiki_id,
                        raw_collection_id,
                        kind.as_str(),
                        window_start,
                        raw_candidate,
                        configuration_hash,
                        now,
                    ],
                )?;
                (transaction.last_insert_rowid(), false)
            };
        transaction.commit()?;
        let run_id = sql_u64(raw_run_id, "invalid sync run ID")?;
        let status = self
            .sync_run_status(run_id)?
            .ok_or(StoreError::CorruptMetadata("new sync run was not found"))?;
        Ok(StartedSyncRun { status, resumed })
    }

    /// Atomically claims a new or restartable authenticated current-page dump import.
    ///
    /// The import is bound to the exact dump digest and length, the collection's
    /// current generation, the owning run's immutable configuration hash, and the
    /// bootstrap race-window timestamp. A restart with any different binding is
    /// rejected rather than silently discarding or reinterpreting the cursor. A run
    /// that already has synchronization jobs but no dump-import identity is also
    /// rejected, because it belongs to a different bootstrap coordinator.
    pub fn claim_or_resume_dump_import(
        &mut self,
        request: DumpImportRequest<'_>,
    ) -> Result<StartedDumpImport, StoreError> {
        validate_dump_digest(request.dump_digest)?;
        if request.dump_compressed_bytes == 0 {
            return Err(StoreError::InvalidDumpIdentity(
                "authenticated dump length must be positive",
            ));
        }
        if request.collection_generation == 0 {
            return Err(StoreError::InvalidDumpIdentity(
                "collection generation must be positive",
            ));
        }
        let raw_run_id = to_sql_integer(request.run_id)?;
        let raw_dump_bytes = to_sql_integer(request.dump_compressed_bytes)?;
        let raw_generation = to_sql_integer(request.collection_generation)?;
        let raw_bootstrap_started_at = to_sql_integer(request.bootstrap_started_at)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run: Option<DumpImportRunBinding> = transaction
            .query_row(
                "SELECT wiki_id, collection_id, run_kind, state,
                        checkpoint_candidate, configuration_hash
                 FROM sync_runs WHERE run_id = ?1",
                [raw_run_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((raw_wiki_id, raw_collection_id, run_kind, run_state, candidate, run_hash)) = run
        else {
            return Err(StoreError::SyncRunNotRunning(request.run_id));
        };
        if run_state != SyncRunState::Running.as_str() {
            return Err(StoreError::SyncRunNotRunning(request.run_id));
        }
        let Some(raw_collection_id) = raw_collection_id else {
            return Err(StoreError::DumpImportRequiresCollectionBootstrap(
                request.run_id,
            ));
        };
        if run_kind != SyncRunKind::Bootstrap.as_str() {
            return Err(StoreError::DumpImportRequiresCollectionBootstrap(
                request.run_id,
            ));
        }
        if candidate != raw_bootstrap_started_at {
            return Err(StoreError::DumpImportBootstrapStartMismatch {
                run_id: request.run_id,
                expected: sql_u64(candidate, "invalid bootstrap checkpoint candidate")?,
                actual: request.bootstrap_started_at,
            });
        }
        let wiki_id = sql_id(raw_wiki_id, "invalid dump import wiki ID")?;
        let collection_id = sql_id(raw_collection_id, "invalid dump import collection ID")?;
        let configuration_hash =
            run_hash.ok_or(StoreError::SyncRunConfigurationUnavailable(request.run_id))?;
        let (current_generation, status): (i64, String) = transaction.query_row(
            "SELECT generation, status FROM collections WHERE collection_id = ?1",
            [raw_collection_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if stored_collection_status(&status)? == CollectionStatus::Tombstoned {
            return Err(StoreError::CollectionTombstoned(collection_id));
        }
        if current_generation != raw_generation {
            return Err(StoreError::StaleCollectionGeneration {
                collection_id,
                expected: request.collection_generation,
                actual: sql_u64(current_generation, "invalid collection generation")?,
            });
        }
        let current_hash =
            manifest_configuration_hash_for(&transaction, wiki_id, Some(collection_id))?;
        if current_hash != configuration_hash {
            return Err(StoreError::StaleDumpImportConfiguration {
                run_id: request.run_id,
            });
        }

        let existing: Option<DumpImportRow> = transaction
            .query_row(
                &format!("{} WHERE run_id = ?1", dump_import_status_query()),
                [raw_run_id],
                dump_import_status_row,
            )
            .optional()?;
        let (raw_import_id, resumed) = if let Some(existing) = existing {
            let import_id = sql_u64(existing.import_id, "invalid dump import ID")?;
            if existing.wiki_id != raw_wiki_id
                || existing.collection_id != raw_collection_id
                || existing.dump_digest != request.dump_digest
                || existing.dump_compressed_bytes != raw_dump_bytes
                || existing.collection_generation != raw_generation
                || existing.configuration_hash != configuration_hash
                || existing.bootstrap_started_at != raw_bootstrap_started_at
            {
                return Err(StoreError::DumpImportIdentityMismatch { import_id });
            }
            match DumpImportState::from_database(&existing.state)? {
                DumpImportState::Succeeded => (existing.import_id, true),
                DumpImportState::Failed if !existing.retryable => {
                    return Err(StoreError::DumpImportNotRestartable(import_id));
                }
                DumpImportState::Running | DumpImportState::Failed => {
                    let next_attempt = u32::try_from(existing.attempt_count)
                        .ok()
                        .and_then(|attempt| attempt.checked_add(1))
                        .ok_or(StoreError::DumpImportProgressOverflow)?;
                    transaction.execute(
                        "UPDATE dump_imports
                         SET state = 'running', attempt_count = ?2,
                             retryable = 1, error_code = NULL, error_message = NULL,
                             claimed_at = ?3, updated_at = ?3, finished_at = NULL
                         WHERE import_id = ?1",
                        params![existing.import_id, next_attempt, now],
                    )?;
                    (existing.import_id, true)
                }
            }
        } else {
            let existing_jobs: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM sync_jobs WHERE run_id = ?1",
                [raw_run_id],
                |row| row.get(0),
            )?;
            if existing_jobs != 0 {
                return Err(StoreError::DumpImportRunHasExistingJobs {
                    run_id: request.run_id,
                    jobs: sql_u64(existing_jobs, "invalid existing sync-job count")?,
                });
            }
            transaction.execute(
                "INSERT INTO dump_imports (
                    run_id, wiki_id, collection_id, dump_digest,
                    dump_compressed_bytes, collection_generation, configuration_hash,
                    bootstrap_started_at, state, created_at, claimed_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                           'running', ?9, ?9, ?9)",
                params![
                    raw_run_id,
                    raw_wiki_id,
                    raw_collection_id,
                    request.dump_digest,
                    raw_dump_bytes,
                    raw_generation,
                    configuration_hash,
                    raw_bootstrap_started_at,
                    now,
                ],
            )?;
            (transaction.last_insert_rowid(), false)
        };
        let raw = transaction.query_row(
            &format!("{} WHERE import_id = ?1", dump_import_status_query()),
            [raw_import_id],
            dump_import_status_row,
        )?;
        let status = stored_dump_import_status(raw)?;
        transaction.commit()?;
        Ok(StartedDumpImport { status, resumed })
    }

    /// Durably advances the sequential page cursor without recording an import.
    pub fn record_dump_import_progress(
        &mut self,
        import_id: u64,
        pages_scanned: u64,
    ) -> Result<DumpImportStatus, StoreError> {
        let raw_import_id = to_sql_integer(import_id)?;
        let raw_pages_scanned = to_sql_integer(pages_scanned)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_dump_progress_can_advance(&transaction, import_id, raw_import_id, pages_scanned)?;
        transaction.execute(
            "UPDATE dump_imports SET pages_scanned = ?2, updated_at = ?3
             WHERE import_id = ?1 AND state = 'running'",
            params![raw_import_id, raw_pages_scanned, now],
        )?;
        let status = dump_import_status_by_id(&transaction, raw_import_id)?;
        transaction.commit()?;
        Ok(status)
    }

    /// Atomically advances the cursor and idempotently records one selected page.
    ///
    /// The page and revision must already be durable, belong to the import's wiki,
    /// and remain active in the bound collection. Repeating an identical record only
    /// advances the cursor; a different revision or byte length for the same page is
    /// rejected.
    pub fn record_dump_imported_page(
        &mut self,
        import_id: u64,
        pages_scanned: u64,
        page_id: PageId,
        revision_id: RevisionId,
        canonical_bytes: u64,
    ) -> Result<DumpImportStatus, StoreError> {
        let raw_import_id = to_sql_integer(import_id)?;
        let raw_pages_scanned = to_sql_integer(pages_scanned)?;
        let raw_page_id = to_sql_integer(page_id.get())?;
        let raw_revision_id = to_sql_integer(revision_id.get())?;
        let raw_canonical_bytes = to_sql_integer(canonical_bytes)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (raw_wiki_id, raw_collection_id, current_pages, current_bytes) =
            ensure_dump_progress_can_advance(
                &transaction,
                import_id,
                raw_import_id,
                pages_scanned,
            )?;
        let existing: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT revision_id, canonical_bytes FROM dump_import_pages
                 WHERE import_id = ?1 AND page_id = ?2",
                params![raw_import_id, raw_page_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (imported_pages, imported_bytes) = if let Some((revision, bytes)) = existing {
            if revision != raw_revision_id || bytes != raw_canonical_bytes {
                return Err(StoreError::ConflictingDumpImportPage { page_id });
            }
            (current_pages, current_bytes)
        } else {
            let selected_and_captured: bool = transaction.query_row(
                "SELECT EXISTS (
                    SELECT 1
                    FROM collection_resolved_members AS member
                    JOIN revisions AS revision
                      ON revision.wiki_id = member.wiki_id
                     AND revision.page_id = member.page_id
                    WHERE member.collection_id = ?1
                      AND member.wiki_id = ?2
                      AND member.page_id = ?3
                      AND member.membership_state = 'active'
                      AND revision.revision_id = ?4
                      AND revision.source_size = ?5
                 )",
                params![
                    raw_collection_id,
                    raw_wiki_id,
                    raw_page_id,
                    raw_revision_id,
                    raw_canonical_bytes,
                ],
                |row| row.get(0),
            )?;
            if !selected_and_captured {
                return Err(StoreError::InvalidDumpImportPage {
                    page_id,
                    revision_id,
                });
            }
            transaction.execute(
                "INSERT INTO dump_import_pages (
                    import_id, wiki_id, page_id, revision_id, canonical_bytes, recorded_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    raw_import_id,
                    raw_wiki_id,
                    raw_page_id,
                    raw_revision_id,
                    raw_canonical_bytes,
                    now,
                ],
            )?;
            (
                current_pages
                    .checked_add(1)
                    .ok_or(StoreError::DumpImportProgressOverflow)?,
                current_bytes
                    .checked_add(raw_canonical_bytes)
                    .ok_or(StoreError::DumpImportProgressOverflow)?,
            )
        };
        transaction.execute(
            "UPDATE dump_imports
             SET pages_scanned = ?2, imported_pages = ?3,
                 imported_canonical_bytes = ?4, updated_at = ?5
             WHERE import_id = ?1 AND state = 'running'",
            params![
                raw_import_id,
                raw_pages_scanned,
                imported_pages,
                imported_bytes,
                now,
            ],
        )?;
        let status = dump_import_status_by_id(&transaction, raw_import_id)?;
        transaction.commit()?;
        Ok(status)
    }

    /// Atomically marks a fully scanned authenticated dump set successful.
    pub fn complete_dump_import(
        &mut self,
        import_id: u64,
        pages_scanned: u64,
    ) -> Result<DumpImportStatus, StoreError> {
        let raw_import_id = to_sql_integer(import_id)?;
        let raw_pages_scanned = to_sql_integer(pages_scanned)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_dump_progress_can_advance(&transaction, import_id, raw_import_id, pages_scanned)?;
        transaction.execute(
            "UPDATE dump_imports
             SET state = 'succeeded', pages_scanned = ?2, retryable = 0,
                 updated_at = ?3, finished_at = ?3
             WHERE import_id = ?1 AND state = 'running'",
            params![raw_import_id, raw_pages_scanned, now],
        )?;
        let status = dump_import_status_by_id(&transaction, raw_import_id)?;
        transaction.commit()?;
        Ok(status)
    }

    /// Atomically retains a structured dump-import failure for status and restart.
    ///
    /// A non-retryable failure also cancels the owning running synchronization run in
    /// this same transaction. Callers must not separately cancel that run afterward.
    pub fn fail_dump_import(
        &mut self,
        import_id: u64,
        code: &str,
        message: &str,
        retryable: bool,
    ) -> Result<DumpImportStatus, StoreError> {
        validate_sync_text(code, "dump import error code")?;
        validate_sync_text(message, "dump import error message")?;
        let raw_import_id = to_sql_integer(import_id)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw_run_id: Option<i64> = transaction
            .query_row(
                "SELECT run_id FROM dump_imports
                 WHERE import_id = ?1 AND state = 'running'",
                [raw_import_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw_run_id) = raw_run_id else {
            return Err(StoreError::DumpImportNotRunning(import_id));
        };
        transaction.execute(
            "UPDATE dump_imports
             SET state = 'failed', retryable = ?2, error_code = ?3,
                 error_message = ?4, updated_at = ?5, finished_at = ?5
             WHERE import_id = ?1 AND state = 'running'",
            params![raw_import_id, retryable, code, message, now],
        )?;
        transaction.execute(
            "INSERT INTO sync_errors (
                run_id, job_id, code, message, retryable, occurred_at
             ) VALUES (?1, NULL, ?2, ?3, ?4, ?5)",
            params![raw_run_id, code, message, retryable, now],
        )?;
        if !retryable {
            let changed = transaction.execute(
                "UPDATE sync_runs SET state = 'cancelled', finished_at = ?2
                 WHERE run_id = ?1 AND state = 'running'",
                params![raw_run_id, now],
            )?;
            if changed != 1 {
                return Err(StoreError::SyncRunNotRunning(sql_u64(
                    raw_run_id,
                    "invalid dump import sync-run ID",
                )?));
            }
        }
        let status = dump_import_status_by_id(&transaction, raw_import_id)?;
        transaction.commit()?;
        Ok(status)
    }

    /// Returns durable dump-import status for one synchronization run.
    pub fn dump_import_status(&self, run_id: u64) -> Result<Option<DumpImportStatus>, StoreError> {
        self.connection
            .query_row(
                &format!("{} WHERE run_id = ?1", dump_import_status_query()),
                [to_sql_integer(run_id)?],
                dump_import_status_row,
            )
            .optional()?
            .map(stored_dump_import_status)
            .transpose()
    }

    /// Adds one idempotent job to a running synchronization operation.
    ///
    /// Repeating the same key and payload returns the original job. Reusing a key
    /// for different work is rejected.
    pub fn enqueue_sync_job(
        &mut self,
        run_id: u64,
        key: &str,
        kind: &str,
        subject: Option<&str>,
    ) -> Result<SyncJob, StoreError> {
        validate_sync_text(key, "sync job key")?;
        validate_sync_text(kind, "sync job kind")?;
        if let Some(subject) = subject {
            validate_sync_text(subject, "sync job subject")?;
        }
        let raw_run_id = to_sql_integer(run_id)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let running: bool = transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM sync_runs WHERE run_id = ?1 AND state = 'running'
             )",
            [raw_run_id],
            |row| row.get(0),
        )?;
        if !running {
            return Err(StoreError::SyncRunNotRunning(run_id));
        }
        transaction.execute(
            "INSERT OR IGNORE INTO sync_jobs (
                run_id, job_key, job_kind, subject, state, created_at
             ) VALUES (?1, ?2, ?3, ?4, 'queued', ?5)",
            params![raw_run_id, key, kind, subject, now],
        )?;
        let raw = transaction.query_row(
            "SELECT job_id, run_id, job_key, job_kind, subject, state,
                    attempt_count, retryable
             FROM sync_jobs WHERE run_id = ?1 AND job_key = ?2",
            params![raw_run_id, key],
            sync_job_row,
        )?;
        let job = stored_sync_job(raw)?;
        if job.kind != kind || job.subject.as_deref() != subject {
            return Err(StoreError::ConflictingSyncJobKey(key.to_owned()));
        }
        transaction.commit()?;
        Ok(job)
    }

    /// Claims the next queued job in stable insertion order.
    pub fn claim_next_sync_job(&mut self, run_id: u64) -> Result<Option<SyncJob>, StoreError> {
        let raw_run_id = to_sql_integer(run_id)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let running: bool = transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM sync_runs WHERE run_id = ?1 AND state = 'running'
             )",
            [raw_run_id],
            |row| row.get(0),
        )?;
        if !running {
            return Err(StoreError::SyncRunNotRunning(run_id));
        }
        let raw_job_id: Option<i64> = transaction
            .query_row(
                "SELECT job_id FROM sync_jobs
                 WHERE run_id = ?1 AND state = 'queued' ORDER BY job_id LIMIT 1",
                [raw_run_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw_job_id) = raw_job_id else {
            transaction.commit()?;
            return Ok(None);
        };
        transaction.execute(
            "UPDATE sync_jobs
             SET state = 'running', attempt_count = attempt_count + 1,
                 started_at = ?2, finished_at = NULL
             WHERE job_id = ?1 AND state = 'queued'",
            params![raw_job_id, now],
        )?;
        let raw = transaction.query_row(
            "SELECT job_id, run_id, job_key, job_kind, subject, state,
                    attempt_count, retryable
             FROM sync_jobs WHERE job_id = ?1",
            [raw_job_id],
            sync_job_row,
        )?;
        transaction.commit()?;
        stored_sync_job(raw).map(Some)
    }

    /// Marks a claimed job successful after its source data is durable.
    pub fn complete_sync_job(&mut self, job_id: u64) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE sync_jobs SET state = 'succeeded', finished_at = ?2
             WHERE job_id = ?1 AND state = 'running'",
            params![to_sql_integer(job_id)?, unix_time()?],
        )?;
        if changed != 1 {
            return Err(StoreError::SyncJobNotRunning(job_id));
        }
        Ok(())
    }

    /// Records a structured job failure without advancing its run checkpoint.
    pub fn fail_sync_job(
        &mut self,
        job_id: u64,
        code: &str,
        message: &str,
        retryable: bool,
    ) -> Result<(), StoreError> {
        validate_sync_text(code, "sync error code")?;
        validate_sync_text(message, "sync error message")?;
        let raw_job_id = to_sql_integer(job_id)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw_run_id: Option<i64> = transaction
            .query_row(
                "SELECT run_id FROM sync_jobs WHERE job_id = ?1 AND state = 'running'",
                [raw_job_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw_run_id) = raw_run_id else {
            return Err(StoreError::SyncJobNotRunning(job_id));
        };
        transaction.execute(
            "UPDATE sync_jobs
             SET state = 'failed', retryable = ?2, finished_at = ?3
             WHERE job_id = ?1",
            params![raw_job_id, retryable, now],
        )?;
        transaction.execute(
            "INSERT INTO sync_errors (
                run_id, job_id, code, message, retryable, occurred_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![raw_run_id, raw_job_id, code, message, retryable, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Commits a run's checkpoint only when every durable job has succeeded.
    pub fn complete_sync_run(
        &mut self,
        run_id: u64,
        recent_changes_cursor: Option<&str>,
    ) -> Result<SyncRunStatus, StoreError> {
        if let Some(cursor) = recent_changes_cursor {
            validate_sync_text(cursor, "RecentChanges cursor")?;
        }
        let raw_run_id = to_sql_integer(run_id)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run: Option<(i64, Option<i64>, i64, String)> = transaction
            .query_row(
                "SELECT wiki_id, collection_id, checkpoint_candidate, run_kind FROM sync_runs
                 WHERE run_id = ?1 AND state = 'running'",
                [raw_run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((raw_wiki_id, raw_collection_id, candidate, run_kind)) = run else {
            return Err(StoreError::SyncRunNotRunning(run_id));
        };
        let incomplete_jobs: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM sync_jobs
             WHERE run_id = ?1 AND state != 'succeeded'",
            [raw_run_id],
            |row| row.get(0),
        )?;
        let incomplete_imports: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM dump_imports
             WHERE run_id = ?1 AND state != 'succeeded'",
            [raw_run_id],
            |row| row.get(0),
        )?;
        let incomplete = incomplete_jobs
            .checked_add(incomplete_imports)
            .ok_or(StoreError::DumpImportProgressOverflow)?;
        if incomplete != 0 {
            return Err(StoreError::IncompleteSyncRun {
                run_id,
                incomplete_jobs: sql_u64(incomplete, "invalid incomplete job count")?,
            });
        }
        let reconciled_at = (run_kind == SyncRunKind::Reconciliation.as_str()).then_some(candidate);
        transaction.execute(
            "UPDATE sync_checkpoints SET
                committed_through = MAX(committed_through, ?3),
                recent_changes_cursor = COALESCE(?4, recent_changes_cursor),
                reconciled_at = COALESCE(?5, reconciled_at),
                last_run_id = ?6,
                updated_at = ?7
             WHERE wiki_id = ?1 AND collection_id IS ?2",
            params![
                raw_wiki_id,
                raw_collection_id,
                candidate,
                recent_changes_cursor,
                reconciled_at,
                raw_run_id,
                now,
            ],
        )?;
        transaction.execute(
            "UPDATE sync_runs SET state = 'succeeded', finished_at = ?2
             WHERE run_id = ?1",
            params![raw_run_id, now],
        )?;
        transaction.commit()?;
        self.sync_run_status(run_id)?
            .ok_or(StoreError::CorruptMetadata(
                "completed sync run was not found",
            ))
    }

    /// Cancels a running operation without changing its source checkpoint.
    pub fn cancel_sync_run(&mut self, run_id: u64) -> Result<(), StoreError> {
        let raw_run_id = to_sql_integer(run_id)?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE sync_runs SET state = 'cancelled', finished_at = ?2
             WHERE run_id = ?1 AND state = 'running'",
            params![raw_run_id, now],
        )?;
        if changed != 1 {
            return Err(StoreError::SyncRunNotRunning(run_id));
        }
        transaction.execute(
            "UPDATE dump_imports
             SET state = 'failed', retryable = 0,
                 error_code = 'sync-run-cancelled',
                 error_message = 'owning synchronization run was cancelled',
                 updated_at = ?2, finished_at = ?2
             WHERE run_id = ?1 AND state = 'running'",
            params![raw_run_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Configures the positive overlap used by future runs for one source.
    pub fn set_sync_overlap(
        &mut self,
        wiki_id: WikiId,
        overlap_seconds: u64,
    ) -> Result<(), StoreError> {
        self.set_sync_overlap_for_collection(wiki_id, None, overlap_seconds)
    }

    /// Configures overlap for a whole source or one collection-scoped cursor.
    pub fn set_sync_overlap_for_collection(
        &mut self,
        wiki_id: WikiId,
        collection_id: Option<CollectionId>,
        overlap_seconds: u64,
    ) -> Result<(), StoreError> {
        if overlap_seconds == 0 {
            return Err(StoreError::InvalidConfig(
                "sync overlap must be greater than zero",
            ));
        }
        if let Some(collection_id) = collection_id {
            let raw_collection_id = to_sql_integer(collection_id.get())?;
            let collection_wiki_id: Option<i64> = self
                .connection
                .query_row(
                    "SELECT wiki_id FROM collections WHERE collection_id = ?1",
                    [raw_collection_id],
                    |row| row.get(0),
                )
                .optional()?;
            if collection_wiki_id != Some(to_sql_integer(wiki_id.get())?) {
                return Err(StoreError::CollectionWikiMismatch);
            }
            ensure_collection_active(&self.connection, collection_id, raw_collection_id)?;
        }
        let changed = self.connection.execute(
            "INSERT INTO sync_checkpoints (
                wiki_id, collection_id, committed_through, overlap_seconds, updated_at
             ) VALUES (?1, ?2, 0, ?3, ?4)
             ON CONFLICT DO UPDATE SET
                 overlap_seconds = excluded.overlap_seconds,
                 updated_at = excluded.updated_at",
            params![
                to_sql_integer(wiki_id.get())?,
                collection_id
                    .map(|id| to_sql_integer(id.get()))
                    .transpose()?,
                to_sql_integer(overlap_seconds)?,
                unix_time()?,
            ],
        )?;
        debug_assert_eq!(changed, 1);
        Ok(())
    }

    /// Returns all persisted source checkpoints in wiki order.
    pub fn sync_checkpoints(&self) -> Result<Vec<SyncCheckpoint>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT wiki_id, collection_id, committed_through, overlap_seconds,
                    recent_changes_cursor, reconciled_at, last_run_id, updated_at
             FROM sync_checkpoints ORDER BY wiki_id, collection_id",
        )?;
        let rows = statement
            .query_map([], sync_checkpoint_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(stored_sync_checkpoint).collect()
    }

    /// Returns bounded recent run summaries, newest first.
    pub fn sync_run_statuses(&self, limit: u32) -> Result<Vec<SyncRunStatus>, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "sync status limit must be between 1 and 100",
            ));
        }
        let mut statement = self.connection.prepare(&format!(
            "{} ORDER BY runs.run_id DESC LIMIT ?1",
            sync_run_status_query()
        ))?;
        let rows = statement
            .query_map([limit], sync_run_status_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(stored_sync_run_status).collect()
    }

    /// Returns bounded unfinished collection reconciliations, oldest first.
    ///
    /// The daemon uses this to resume durable scheduled or interrupted work before
    /// claiming a later schedule occurrence. Source-wide and bootstrap runs are not
    /// included because they require a different application operation.
    pub fn running_collection_reconciliations(
        &self,
        limit: u32,
    ) -> Result<Vec<SyncRunStatus>, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "sync status limit must be between 1 and 100",
            ));
        }
        let mut statement = self.connection.prepare(&format!(
            "{} WHERE runs.state = 'running'
                 AND runs.run_kind = 'reconciliation'
                 AND runs.collection_id IS NOT NULL
             ORDER BY runs.run_id ASC LIMIT ?1",
            sync_run_status_query()
        ))?;
        let rows = statement
            .query_map([limit], sync_run_status_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(stored_sync_run_status).collect()
    }

    /// Returns one aggregate run status by local identity.
    pub fn sync_run_status(&self, run_id: u64) -> Result<Option<SyncRunStatus>, StoreError> {
        self.connection
            .query_row(
                &format!("{} WHERE runs.run_id = ?1", sync_run_status_query()),
                [to_sql_integer(run_id)?],
                sync_run_status_row,
            )
            .optional()?
            .map(stored_sync_run_status)
            .transpose()
    }

    /// Returns the number of canonical manifest files after validating their names.
    pub fn manifest_count(&self) -> Result<u64, StoreError> {
        u64::try_from(self.manifest_sequences()?.len())
            .map_err(|_| StoreError::ManifestLimitExceeded)
    }

    /// Reads and identity-verifies one canonical manifest by append sequence.
    pub fn read_manifest(&self, sequence: u64) -> Result<StoredManifest, StoreError> {
        if sequence == 0 {
            return Err(StoreError::InvalidManifest(
                "manifest sequence must be positive",
            ));
        }
        let path = self.manifest_path(sequence)?;
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                StoreError::ManifestNotFound(sequence)
            } else {
                StoreError::Io(error)
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StoreError::CorruptManifest {
                sequence,
                message: "manifest path is not a regular file",
            });
        }
        if metadata.len() > MAX_MANIFEST_BYTES {
            return Err(StoreError::CorruptManifest {
                sequence,
                message: "manifest exceeds the file-size bound",
            });
        }
        let file = File::open(path)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(StoreError::CorruptManifest {
                sequence,
                message: "manifest exceeds the file-size bound",
            });
        }
        decode_manifest(sequence, &bytes)
    }

    /// Returns a bounded page of manifests in increasing sequence order.
    pub fn manifests_after(
        &self,
        after_sequence: Option<u64>,
        limit: u32,
    ) -> Result<Vec<StoredManifest>, StoreError> {
        if !(1..=MAX_MANIFEST_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "manifest page size must be between 1 and 1,000",
            ));
        }
        self.manifest_sequences()?
            .into_iter()
            .filter(|sequence| after_sequence.is_none_or(|after| *sequence > after))
            .take(limit as usize)
            .map(|sequence| self.read_manifest(sequence))
            .collect()
    }

    /// Appends the canonical manifest for one successful durable sync run.
    ///
    /// All manifest fields are derived from durable SQLite state. Repeating the call
    /// for an already represented run returns the existing manifest. Running,
    /// cancelled, and unknown runs are rejected. The database commit and file append
    /// are deliberately separate durability boundaries: after a crash between them,
    /// [`Library::append_missing_sync_manifests`] repairs the detectable gap.
    pub fn append_sync_manifest(&mut self, run_id: u64) -> Result<StoredManifest, StoreError> {
        self.ensure_writable()?;
        let existing = self
            .validated_manifest_chain()?
            .into_iter()
            .find(|stored| stored.manifest.run_id == run_id);
        if let Some(existing) = existing {
            return Ok(existing);
        }

        let status = self
            .sync_run_status(run_id)?
            .ok_or(StoreError::SyncRunNotSucceeded(run_id))?;
        if status.state != SyncRunState::Succeeded {
            return Err(StoreError::SyncRunNotSucceeded(run_id));
        }
        let expected_run_id = self
            .unmanifested_succeeded_run_ids(1)?
            .into_iter()
            .next()
            .ok_or(StoreError::CorruptMetadata(
                "successful unmanifested run was not found",
            ))?;
        if run_id != expected_run_id {
            return Err(StoreError::ManifestRunOutOfOrder {
                expected: expected_run_id,
                requested: run_id,
            });
        }
        status.finished_at.ok_or(StoreError::CorruptMetadata(
            "successful sync run lacks finish time",
        ))?;
        let source = self
            .wiki(status.wiki_id)?
            .ok_or(StoreError::WikiNotFound(status.wiki_id))?
            .api_endpoint;
        validate_manifest_text(&source)?;
        let configuration_hash = status
            .configuration_hash
            .clone()
            .ok_or(StoreError::SyncRunConfigurationUnavailable(run_id))?;
        let prior = self.validated_manifest_chain()?;
        let mut introduced_revisions =
            self.manifest_catalog_revisions(status.wiki_id, status.collection_id)?;
        let mut candidate_revision_ids = introduced_revisions
            .iter()
            .map(|revision| revision.revision_id)
            .collect::<HashSet<_>>();
        for stored in &prior {
            if stored.manifest.wiki_id == status.wiki_id {
                for revision in &stored.manifest.introduced_revisions {
                    candidate_revision_ids.remove(&revision.revision_id);
                }
            }
        }
        introduced_revisions
            .retain(|revision| candidate_revision_ids.contains(&revision.revision_id));
        let page_heads = self.manifest_page_heads(status.wiki_id, status.collection_id)?;
        let media_snapshot = self.manifest_media_snapshot(status.wiki_id, status.collection_id)?;
        let sequence = prior.last().map_or(Ok(1_u64), |stored| {
            stored
                .manifest
                .sequence
                .checked_add(1)
                .ok_or(StoreError::ManifestLimitExceeded)
        })?;
        let predecessor = prior.last().map(|stored| stored.id);
        let manifest = SyncManifest {
            sequence,
            predecessor,
            run_id,
            wiki_id: status.wiki_id,
            collection_id: status.collection_id,
            run_kind: status.kind,
            source,
            capture_started_at: status.window_start,
            capture_completed_at: status.checkpoint_candidate,
            configuration_hash,
            introduced_revisions,
            page_heads,
            media_snapshot: Some(media_snapshot),
        };
        let (id, bytes) = encode_manifest(&manifest)?;
        let path = self.manifest_path(sequence)?;
        if path.exists() {
            return Err(StoreError::ManifestConflict(sequence));
        }
        let mut temporary = tempfile::Builder::new()
            .prefix("manifest-")
            .tempfile_in(self.root.join("tmp"))?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        #[cfg(unix)]
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o600))?;
        temporary
            .persist(&path)
            .map_err(|error| StoreError::Io(error.error))?;
        sync_directory(&self.root.join(MANIFEST_DIRECTORY))?;
        let stored = self.read_manifest(sequence)?;
        if stored.id != id {
            return Err(StoreError::CorruptManifest {
                sequence,
                message: "installed manifest identity changed",
            });
        }
        Ok(stored)
    }

    /// Appends manifests for a bounded oldest-first set of successful unrepresented runs.
    pub fn append_missing_sync_manifests(
        &mut self,
        limit: u32,
    ) -> Result<Vec<StoredManifest>, StoreError> {
        let run_ids = self.unmanifested_succeeded_run_ids(limit)?;
        run_ids
            .into_iter()
            .map(|run_id| self.append_sync_manifest(run_id))
            .collect()
    }

    /// Finds a bounded oldest-first set of successful runs with no installed manifest.
    pub fn unmanifested_succeeded_run_ids(&self, limit: u32) -> Result<Vec<u64>, StoreError> {
        if !(1..=MAX_MANIFEST_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "missing manifest limit must be between 1 and 1,000",
            ));
        }
        let represented = self
            .validated_manifest_chain()?
            .into_iter()
            .map(|stored| stored.manifest.run_id)
            .collect::<HashSet<_>>();
        let mut statement = self.connection.prepare(
            "SELECT run_id FROM sync_runs
                 WHERE state = 'succeeded' AND configuration_hash IS NOT NULL
                 ORDER BY run_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
        let mut missing = Vec::new();
        for row in rows {
            let run_id = sql_u64(row?, "invalid sync run ID")?;
            if !represented.contains(&run_id) {
                missing.push(run_id);
                if missing.len() == limit as usize {
                    break;
                }
            }
        }
        Ok(missing)
    }

    /// Returns a bounded increasing page of successful run IDs for integrity scans.
    pub fn succeeded_sync_run_ids_after(
        &self,
        after_run_id: Option<u64>,
        limit: u32,
    ) -> Result<Vec<u64>, StoreError> {
        if !(1..=MAX_MANIFEST_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "successful run page size must be between 1 and 1,000",
            ));
        }
        let after = to_sql_integer(after_run_id.unwrap_or(0))?;
        let mut statement = self.connection.prepare(
            "SELECT run_id FROM sync_runs
             WHERE state = 'succeeded' AND configuration_hash IS NOT NULL AND run_id > ?1
             ORDER BY run_id LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![after, limit], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|value| sql_u64(value, "invalid sync run ID"))
            .collect()
    }

    fn all_manifests(&self) -> Result<Vec<StoredManifest>, StoreError> {
        self.manifest_sequences()?
            .into_iter()
            .map(|sequence| self.read_manifest(sequence))
            .collect()
    }

    fn validated_manifest_chain(&self) -> Result<Vec<StoredManifest>, StoreError> {
        let manifests = self.all_manifests()?;
        let mut previous = None;
        let mut run_ids = HashSet::new();
        for (index, stored) in manifests.iter().enumerate() {
            let expected_sequence = index as u64 + 1;
            if stored.manifest.sequence != expected_sequence {
                return Err(StoreError::CorruptManifest {
                    sequence: expected_sequence,
                    message: "manifest append sequence has a gap",
                });
            }
            if stored.manifest.predecessor != previous {
                return Err(StoreError::CorruptManifest {
                    sequence: expected_sequence,
                    message: "manifest predecessor chain is broken",
                });
            }
            if !run_ids.insert(stored.manifest.run_id) {
                return Err(StoreError::CorruptManifest {
                    sequence: expected_sequence,
                    message: "sync run occurs more than once in manifest chain",
                });
            }
            previous = Some(stored.id);
        }
        Ok(manifests)
    }

    fn manifest_sequences(&self) -> Result<Vec<u64>, StoreError> {
        let directory = self.root.join(MANIFEST_DIRECTORY);
        let metadata = fs::symlink_metadata(&directory)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::InvalidManifest(
                "manifest directory is not a regular directory",
            ));
        }
        let mut sequences = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::InvalidManifest("manifest filename is not UTF-8"))?;
            let sequence = parse_manifest_filename(&name)?;
            sequences.push(sequence);
            if sequences.len() > MAX_MANIFEST_ENTRIES {
                return Err(StoreError::ManifestLimitExceeded);
            }
        }
        sequences.sort_unstable();
        if sequences.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(StoreError::InvalidManifest("duplicate manifest sequence"));
        }
        Ok(sequences)
    }

    fn manifest_path(&self, sequence: u64) -> Result<PathBuf, StoreError> {
        if sequence == 0 || sequence >= 10_u64.pow(MANIFEST_FILENAME_DIGITS as u32) {
            return Err(StoreError::ManifestLimitExceeded);
        }
        Ok(self.root.join(MANIFEST_DIRECTORY).join(format!(
            "{sequence:0width$}.json",
            width = MANIFEST_FILENAME_DIGITS
        )))
    }

    fn manifest_catalog_revisions(
        &self,
        wiki_id: WikiId,
        collection_id: Option<CollectionId>,
    ) -> Result<Vec<ManifestRevision>, StoreError> {
        let sql = if collection_id.is_some() {
            "SELECT revisions.page_id, revisions.revision_id,
                    revisions.content_object_id
             FROM revisions
             JOIN collection_resolved_members members
               ON members.wiki_id = revisions.wiki_id
              AND members.page_id = revisions.page_id
             WHERE revisions.wiki_id = ?1
               AND members.collection_id = ?2
             ORDER BY revisions.revision_id"
        } else {
            "SELECT page_id, revision_id, content_object_id FROM revisions
             WHERE wiki_id = ?1 AND ?2 IS NULL
             ORDER BY revision_id"
        };
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map(
            params![
                to_sql_integer(wiki_id.get())?,
                collection_id
                    .map(|id| to_sql_integer(id.get()))
                    .transpose()?
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut revisions = Vec::new();
        for row in rows {
            let (page_id, revision_id, object_id) = row?;
            revisions.push(ManifestRevision {
                page_id: sql_id(page_id, "invalid page ID in manifest input")?,
                revision_id: sql_id(revision_id, "invalid revision ID in manifest input")?,
                content_object_id: object_id.parse().map_err(|_| {
                    StoreError::CorruptMetadata("invalid object ID in manifest input")
                })?,
            });
            if revisions.len() > MAX_MANIFEST_ENTRIES {
                return Err(StoreError::ManifestLimitExceeded);
            }
        }
        Ok(revisions)
    }

    fn manifest_page_heads(
        &self,
        wiki_id: WikiId,
        collection_id: Option<CollectionId>,
    ) -> Result<Vec<ManifestPageHead>, StoreError> {
        let (sql, collection_parameter) = if let Some(collection_id) = collection_id {
            (
                "SELECT pages.page_id, pages.current_revision_id
                 FROM collection_resolved_members members
                 JOIN pages ON pages.wiki_id = members.wiki_id
                           AND pages.page_id = members.page_id
                 WHERE members.collection_id = ?1
                   AND members.membership_state = 'active'
                 ORDER BY pages.page_id",
                to_sql_integer(collection_id.get())?,
            )
        } else {
            (
                "SELECT page_id, current_revision_id FROM pages
                 WHERE wiki_id = ?1 ORDER BY page_id",
                to_sql_integer(wiki_id.get())?,
            )
        };
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map([collection_parameter], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut heads = Vec::new();
        for row in rows {
            let (page_id, revision_id) = row?;
            heads.push(ManifestPageHead {
                page_id: sql_id(page_id, "invalid page ID in manifest head")?,
                revision_id: revision_id
                    .map(|value| sql_id(value, "invalid revision ID in manifest head"))
                    .transpose()?,
            });
            if heads.len() > MAX_MANIFEST_ENTRIES {
                return Err(StoreError::ManifestLimitExceeded);
            }
        }
        Ok(heads)
    }

    /// Returns the complete bounded media inventory represented by a manifest for
    /// the given source or collection scope.
    ///
    /// Collection snapshots include media reachable from every retained resolved
    /// member revision, matching the revision-catalog scope used by manifests.
    pub fn manifest_media_snapshot(
        &self,
        wiki_id: WikiId,
        collection_id: Option<CollectionId>,
    ) -> Result<ManifestMediaSnapshot, StoreError> {
        let raw_wiki_id = to_sql_integer(wiki_id.get())?;
        let raw_collection_id = collection_id
            .map(|id| to_sql_integer(id.get()))
            .transpose()?;
        let placement_sql = if collection_id.is_some() {
            "SELECT placement.revision_id, placement.placement_index,
                    placement.source_media_id, placement.source_sha1,
                    placement.content_object_id, placement.placement_kind,
                    placement.caption, placement.alt_text
             FROM page_media AS placement
             JOIN revisions AS revision
               ON revision.wiki_id = placement.wiki_id
              AND revision.revision_id = placement.revision_id
             JOIN collection_resolved_members AS member
               ON member.wiki_id = revision.wiki_id
              AND member.page_id = revision.page_id
             WHERE member.collection_id = ?2 AND placement.wiki_id = ?1
             ORDER BY placement.revision_id, placement.placement_index"
        } else {
            "SELECT placement.revision_id, placement.placement_index,
                    placement.source_media_id, placement.source_sha1,
                    placement.content_object_id, placement.placement_kind,
                    placement.caption, placement.alt_text
             FROM page_media AS placement
             WHERE placement.wiki_id = ?1 AND ?2 IS NULL
             ORDER BY placement.revision_id, placement.placement_index"
        };
        let mut placement_statement = self.connection.prepare(placement_sql)?;
        let placement_rows =
            placement_statement.query_map(params![raw_wiki_id, raw_collection_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })?;
        let mut placements = Vec::new();
        for row in placement_rows {
            let (revision_id, index, media_id, source_sha1, object_id, kind, caption, alt_text) =
                row?;
            let parsed_revision_id: RevisionId =
                sql_id(revision_id, "invalid manifest media revision ID")?;
            let placement_index = u32::try_from(index)
                .map_err(|_| StoreError::CorruptMetadata("invalid manifest placement index"))?;
            let parsed_media_id: MediaId = sql_id(media_id, "invalid manifest media ID")?;
            let parsed_object_id = object_id
                .parse()
                .map_err(|_| StoreError::CorruptMetadata("invalid manifest media object ID"))?;
            let identity = manifest_record_identity(
                MANIFEST_MEDIA_PLACEMENT_DOMAIN,
                &ManifestMediaPlacementIdentityBody {
                    wiki_id: wiki_id.get(),
                    revision_id: parsed_revision_id.get(),
                    placement_index,
                    media_id: parsed_media_id.get(),
                    source_sha1: &source_sha1,
                    content_object_id: &object_id,
                    placement_kind: &kind,
                    caption: caption.as_deref(),
                    alt_text: alt_text.as_deref(),
                },
            )?;
            placements.push(ManifestMediaPlacement {
                revision_id: parsed_revision_id,
                placement_index,
                media_id: parsed_media_id,
                source_sha1,
                content_object_id: parsed_object_id,
                placement_identity: identity,
            });
            if placements.len() > MAX_MANIFEST_ENTRIES {
                return Err(StoreError::ManifestLimitExceeded);
            }
        }

        let inventory_sql = if collection_id.is_some() {
            "SELECT DISTINCT media.source_media_id, media.source_sha1,
                    media.content_object_id, media.file_title, media.original_url,
                    media.description_url, media.author, media.attribution,
                    media.license_name, media.license_url, media.width, media.height,
                    media.mime_type, media.captured_at
             FROM media
             JOIN page_media AS placement
               ON placement.wiki_id = media.wiki_id
              AND placement.source_media_id = media.source_media_id
              AND placement.source_sha1 = media.source_sha1
              AND placement.content_object_id = media.content_object_id
             JOIN revisions AS revision
               ON revision.wiki_id = placement.wiki_id
              AND revision.revision_id = placement.revision_id
             JOIN collection_resolved_members AS member
               ON member.wiki_id = revision.wiki_id
              AND member.page_id = revision.page_id
             WHERE member.collection_id = ?2 AND media.wiki_id = ?1
             ORDER BY media.source_media_id, media.source_sha1, media.content_object_id"
        } else {
            "SELECT media.source_media_id, media.source_sha1,
                    media.content_object_id, media.file_title, media.original_url,
                    media.description_url, media.author, media.attribution,
                    media.license_name, media.license_url, media.width, media.height,
                    media.mime_type, media.captured_at
             FROM media WHERE media.wiki_id = ?1 AND ?2 IS NULL
             ORDER BY media.source_media_id, media.source_sha1, media.content_object_id"
        };
        let mut inventory_statement = self.connection.prepare(inventory_sql)?;
        let inventory_rows =
            inventory_statement.query_map(params![raw_wiki_id, raw_collection_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, i64>(13)?,
                ))
            })?;
        let mut inventory = Vec::new();
        for row in inventory_rows {
            let (
                media_id,
                source_sha1,
                object_id,
                file_title,
                original_url,
                description_url,
                author,
                attribution,
                license_name,
                license_url,
                width,
                height,
                mime_type,
                captured_at,
            ) = row?;
            let parsed_media_id: MediaId = sql_id(media_id, "invalid manifest media ID")?;
            let parsed_object_id = object_id
                .parse()
                .map_err(|_| StoreError::CorruptMetadata("invalid manifest media object ID"))?;
            let identity = manifest_record_identity(
                MANIFEST_MEDIA_DOMAIN,
                &ManifestMediaIdentityBody {
                    wiki_id: wiki_id.get(),
                    media_id: parsed_media_id.get(),
                    source_sha1: &source_sha1,
                    content_object_id: &object_id,
                    file_title: &file_title,
                    original_url: &original_url,
                    description_url: &description_url,
                    author: &author,
                    attribution: &attribution,
                    license_name: &license_name,
                    license_url: license_url.as_deref(),
                    width,
                    height,
                    mime_type: &mime_type,
                    captured_at,
                },
            )?;
            inventory.push(ManifestMedia {
                media_id: parsed_media_id,
                source_sha1,
                content_object_id: parsed_object_id,
                metadata_identity: identity,
            });
            if inventory.len() > MAX_MANIFEST_ENTRIES {
                return Err(StoreError::ManifestLimitExceeded);
            }
        }
        Ok(ManifestMediaSnapshot {
            inventory,
            placements,
        })
    }

    /// Makes canonical bytes durable, then atomically records their page and revision.
    ///
    /// Repeating the same capture is idempotent. Conflicting immutable metadata for
    /// an existing remote revision is rejected instead of silently rewritten.
    pub fn capture_current_revision(
        &mut self,
        wiki_id: WikiId,
        collection_id: CollectionId,
        capture: &CurrentRevisionCapture<'_>,
    ) -> Result<StoredObject, StoreError> {
        validate_mediawiki_timestamp(capture.timestamp)?;
        let raw_wiki_id = to_sql_integer(wiki_id.get())?;
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let collection_wiki_id: Option<i64> = self
            .connection
            .query_row(
                "SELECT wiki_id FROM collections WHERE collection_id = ?1",
                [raw_collection_id],
                |row| row.get(0),
            )
            .optional()?;
        if collection_wiki_id != Some(raw_wiki_id) {
            return Err(StoreError::CollectionWikiMismatch);
        }
        ensure_collection_active(&self.connection, collection_id, raw_collection_id)?;
        let object = self.put_bytes(ObjectKind::Wikitext, capture.source)?;
        let now = unix_time()?;
        let wiki_id = raw_wiki_id;
        let collection_id = raw_collection_id;
        let page_id = to_sql_integer(capture.page_id.get())?;
        let revision_id = to_sql_integer(capture.revision_id.get())?;
        let revision = RevisionCapture {
            revision_id: capture.revision_id,
            parent_id: capture.parent_id,
            timestamp: capture.timestamp,
            author: capture.author,
            author_id: capture.author_id,
            comment: capture.comment,
            minor: capture.minor,
            upstream_sha1: capture.upstream_sha1,
            content_model: capture.content_model,
            source: capture.source,
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let owns_collection: bool = transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM collections
                WHERE collection_id = ?1 AND wiki_id = ?2
             )",
            params![collection_id, wiki_id],
            |row| row.get(0),
        )?;
        if !owns_collection {
            return Err(StoreError::CollectionWikiMismatch);
        }

        let membership_state: Option<String> = transaction
            .query_row(
                "SELECT membership_state FROM collection_resolved_members
                 WHERE collection_id = ?1 AND page_id = ?2",
                params![collection_id, page_id],
                |row| row.get(0),
            )
            .optional()?;
        if membership_state.as_deref() == Some("removed") {
            return Err(StoreError::CollectionMemberNotActive {
                collection_id: sql_id(collection_id, "invalid collection ID")?,
                page_id: capture.page_id,
            });
        }
        let membership_added = membership_state.is_none();
        if membership_added {
            transaction.execute(
                "INSERT INTO collection_resolved_members (
                    collection_id, wiki_id, page_id, namespace, title,
                    inclusion_kind, inclusion_title, inclusion_depth,
                    membership_state, first_resolved_at, last_resolved_at, removed_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5,
                           'explicit-title', ?5, NULL, 'active', ?6, ?6, NULL)",
                params![
                    collection_id,
                    wiki_id,
                    page_id,
                    capture.namespace,
                    capture.title.as_str(),
                    now,
                ],
            )?;
        }

        transaction.execute(
            "INSERT INTO pages (
                wiki_id, page_id, namespace, current_title, current_revision_id,
                current_revision_time, state, first_captured_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?7)
             ON CONFLICT(wiki_id, page_id) DO UPDATE SET
                namespace = excluded.namespace,
                current_title = excluded.current_title,
                current_revision_id = excluded.current_revision_id,
                current_revision_time = excluded.current_revision_time,
                state = 'active',
                updated_at = excluded.updated_at
             WHERE pages.current_revision_time IS NULL
                OR excluded.current_revision_time >= pages.current_revision_time",
            params![
                wiki_id,
                page_id,
                capture.namespace,
                capture.title.as_str(),
                revision_id,
                capture.timestamp,
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO page_titles (
                wiki_id, page_id, title, first_observed_at, last_observed_at
             ) VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(wiki_id, page_id, title) DO UPDATE SET
                last_observed_at = excluded.last_observed_at",
            params![wiki_id, page_id, capture.title.as_str(), now],
        )?;
        insert_revision(&transaction, wiki_id, page_id, &revision, object.id, now)?;
        transaction.execute(
            "INSERT OR IGNORE INTO collection_pages (
                collection_id, wiki_id, page_id, inclusion_reason, added_at
             ) VALUES (?1, ?2, ?3, 'explicit-title', ?4)",
            params![collection_id, wiki_id, page_id, now],
        )?;
        let unresolved_removed = transaction.execute(
            "DELETE FROM unresolved_titles WHERE collection_id = ?1 AND title = ?2",
            params![collection_id, capture.title.as_str()],
        )?;
        if membership_added || unresolved_removed != 0 {
            let changed = transaction.execute(
                "UPDATE collections SET generation = generation + 1
                 WHERE collection_id = ?1 AND status = 'active'",
                [collection_id],
            )?;
            if changed != 1 {
                return Err(StoreError::CollectionTombstoned(sql_id(
                    collection_id,
                    "invalid collection ID",
                )?));
            }
        }
        transaction.commit()?;
        Ok(object)
    }

    /// Makes one additional revision of an already captured page durable.
    ///
    /// This does not move the page head or update the current-page search index.
    /// Repetition is idempotent and conflicting immutable revision identity is
    /// rejected.
    pub fn capture_revision(
        &mut self,
        wiki_id: WikiId,
        page_id: PageId,
        capture: &RevisionCapture<'_>,
    ) -> Result<StoredObject, StoreError> {
        if self.page(wiki_id, page_id)?.is_none() {
            return Err(StoreError::PageNotFound { wiki_id, page_id });
        }
        validate_mediawiki_timestamp(capture.timestamp)?;
        let object = self.put_bytes(ObjectKind::Wikitext, capture.source)?;
        let now = unix_time()?;
        let raw_wiki_id = to_sql_integer(wiki_id.get())?;
        let raw_page_id = to_sql_integer(page_id.get())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let page_exists: bool = transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM pages WHERE wiki_id = ?1 AND page_id = ?2
             )",
            params![raw_wiki_id, raw_page_id],
            |row| row.get(0),
        )?;
        if !page_exists {
            return Err(StoreError::PageNotFound { wiki_id, page_id });
        }
        insert_revision(
            &transaction,
            raw_wiki_id,
            raw_page_id,
            capture,
            object.id,
            now,
        )?;
        transaction.commit()?;
        Ok(object)
    }

    /// Durably captures one passive raster thumbnail and atomically links it to a
    /// previously captured article revision.
    ///
    /// The article revision must already be durable. Validation and metadata
    /// conflicts are checked before object installation where possible, and any
    /// media failure leaves the text revision and its current-page state unchanged.
    /// Repeating the same rendition and immutable metadata is idempotent even when
    /// the retry supplies a later capture time; the first durable time is retained.
    pub fn capture_revision_thumbnail(
        &mut self,
        wiki_id: WikiId,
        page_id: PageId,
        revision_id: RevisionId,
        policy: ThumbnailPolicy,
        capture: &ThumbnailCapture<'_>,
        placement: RevisionMediaPlacement<'_>,
    ) -> Result<StoredObject, StoreError> {
        self.ensure_writable()?;
        validate_thumbnail_capture(policy, capture, placement)?;
        let raw_wiki_id = to_sql_integer(wiki_id.get())?;
        let raw_page_id = to_sql_integer(page_id.get())?;
        let raw_revision_id = to_sql_integer(revision_id.get())?;
        let raw_media_id = to_sql_integer(capture.media_id.get())?;
        let raw_placement_index = i64::from(placement.index);
        let revision_page: Option<i64> = self
            .connection
            .query_row(
                "SELECT page_id FROM revisions WHERE wiki_id = ?1 AND revision_id = ?2",
                params![raw_wiki_id, raw_revision_id],
                |row| row.get(0),
            )
            .optional()?;
        match revision_page {
            None => return Err(StoreError::RevisionNotFound(revision_id)),
            Some(stored_page_id) if stored_page_id != raw_page_id => {
                return Err(StoreError::RevisionPageMismatch {
                    revision_id,
                    page_id,
                });
            }
            Some(_) => {}
        }

        let object_id = ObjectId::for_bytes(ObjectKind::Media, capture.source);
        let expected_metadata = expected_media_metadata(capture);
        if let Some(existing) = query_media_metadata(
            &self.connection,
            raw_wiki_id,
            raw_media_id,
            capture.source_sha1,
            object_id,
        )? && existing != expected_metadata
        {
            return Err(StoreError::ConflictingMedia(capture.media_id));
        }
        if let Some(existing) = query_media_placement(
            &self.connection,
            raw_wiki_id,
            raw_revision_id,
            raw_placement_index,
        )? && existing
            != (
                raw_media_id,
                capture.source_sha1.to_owned(),
                object_id.to_string(),
                placement.kind.as_str().to_owned(),
                placement.caption.map(str::to_owned),
                placement.alt_text.map(str::to_owned),
            )
        {
            return Err(StoreError::ConflictingMediaPlacement {
                revision_id,
                placement_index: placement.index,
            });
        }

        let object = self.put_bytes(ObjectKind::Media, capture.source)?;
        if object.id != object_id {
            return Err(StoreError::HashMismatch(object_id));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let transaction_page: Option<i64> = transaction
            .query_row(
                "SELECT page_id FROM revisions WHERE wiki_id = ?1 AND revision_id = ?2",
                params![raw_wiki_id, raw_revision_id],
                |row| row.get(0),
            )
            .optional()?;
        if transaction_page != Some(raw_page_id) {
            return Err(StoreError::RevisionPageMismatch {
                revision_id,
                page_id,
            });
        }
        transaction.execute(
            "INSERT OR IGNORE INTO media (
                wiki_id, source_media_id, source_sha1, file_title,
                original_url, description_url, author, attribution,
                license_name, license_url, width, height, mime_type,
                captured_at, content_object_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                       ?11, ?12, ?13, ?14, ?15)",
            params![
                raw_wiki_id,
                raw_media_id,
                capture.source_sha1,
                capture.file_title.as_str(),
                capture.original_url,
                capture.description_url,
                capture.author,
                capture.attribution,
                capture.license_name,
                capture.license_url,
                i64::from(capture.width),
                i64::from(capture.height),
                capture.mime_type.as_str(),
                to_sql_integer(capture.captured_at)?,
                object.id.to_string(),
            ],
        )?;
        let stored_metadata = query_media_metadata(
            &transaction,
            raw_wiki_id,
            raw_media_id,
            capture.source_sha1,
            object_id,
        )?
        .ok_or(StoreError::CorruptMetadata(
            "captured media metadata is absent",
        ))?;
        if stored_metadata != expected_metadata {
            return Err(StoreError::ConflictingMedia(capture.media_id));
        }
        transaction.execute(
            "INSERT OR IGNORE INTO page_media (
                wiki_id, revision_id, placement_index, source_media_id,
                source_sha1, content_object_id, placement_kind, caption, alt_text
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                raw_wiki_id,
                raw_revision_id,
                raw_placement_index,
                raw_media_id,
                capture.source_sha1,
                object.id.to_string(),
                placement.kind.as_str(),
                placement.caption,
                placement.alt_text,
            ],
        )?;
        let stored_placement = query_media_placement(
            &transaction,
            raw_wiki_id,
            raw_revision_id,
            raw_placement_index,
        )?
        .ok_or(StoreError::CorruptMetadata(
            "captured media placement is absent",
        ))?;
        let expected_placement = (
            raw_media_id,
            capture.source_sha1.to_owned(),
            object.id.to_string(),
            placement.kind.as_str().to_owned(),
            placement.caption.map(str::to_owned),
            placement.alt_text.map(str::to_owned),
        );
        if stored_placement != expected_placement {
            return Err(StoreError::ConflictingMediaPlacement {
                revision_id,
                placement_index: placement.index,
            });
        }
        transaction.commit()?;
        Ok(object)
    }

    /// Returns every captured thumbnail placement for one revision in stable order.
    ///
    /// The schema and capture API cap the result at
    /// [`MAX_THUMBNAILS_PER_REVISION`]. Canonical bytes remain in the object store
    /// and are read separately through [`Self::read_object`].
    pub fn revision_media(
        &self,
        wiki_id: WikiId,
        revision_id: RevisionId,
    ) -> Result<Vec<StoredRevisionMedia>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT placements.placement_index, placements.placement_kind,
                    placements.caption, placements.alt_text,
                    media.source_media_id, media.source_sha1, media.file_title,
                    media.original_url, media.description_url, media.author,
                    media.attribution, media.license_name, media.license_url,
                    media.width, media.height, media.mime_type, media.captured_at,
                    media.content_object_id
             FROM page_media AS placements
             JOIN media
               ON media.wiki_id = placements.wiki_id
              AND media.source_media_id = placements.source_media_id
              AND media.source_sha1 = placements.source_sha1
              AND media.content_object_id = placements.content_object_id
             WHERE placements.wiki_id = ?1 AND placements.revision_id = ?2
             ORDER BY placements.placement_index
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                to_sql_integer(wiki_id.get())?,
                to_sql_integer(revision_id.get())?,
                i64::from(MAX_THUMBNAILS_PER_REVISION),
            ],
            revision_media_row,
        )?;
        let mut media = Vec::new();
        for row in rows {
            media.push(stored_revision_media(wiki_id, revision_id, row?)?);
        }
        Ok(media)
    }

    /// Looks up a captured page by stable remote identity.
    pub fn page(&self, wiki_id: WikiId, page_id: PageId) -> Result<Option<StoredPage>, StoreError> {
        self.connection
            .query_row(
                "SELECT namespace, current_title, current_revision_id
                 FROM pages WHERE wiki_id = ?1 AND page_id = ?2",
                params![
                    to_sql_integer(wiki_id.get())?,
                    to_sql_integer(page_id.get())?
                ],
                |row| {
                    let raw_revision: Option<i64> = row.get(2)?;
                    Ok((
                        row.get::<_, i32>(0)?,
                        row.get::<_, String>(1)?,
                        raw_revision,
                    ))
                },
            )
            .optional()?
            .map(|(namespace, title, revision_id)| {
                Ok(StoredPage {
                    wiki_id,
                    page_id,
                    namespace,
                    title: PageTitle::new(title)
                        .map_err(|_| StoreError::CorruptMetadata("invalid stored page title"))?,
                    current_revision_id: revision_id
                        .map(|value| sql_id(value, "invalid stored revision ID"))
                        .transpose()?,
                })
            })
            .transpose()
    }

    /// Lists the pages currently selected by one collection in stable page-ID order.
    pub fn collection_pages(
        &self,
        wiki_id: WikiId,
        collection_id: CollectionId,
    ) -> Result<Vec<StoredPage>, StoreError> {
        let owns_collection: bool = self.connection.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM collections WHERE collection_id = ?1 AND wiki_id = ?2
             )",
            params![
                to_sql_integer(collection_id.get())?,
                to_sql_integer(wiki_id.get())?
            ],
            |row| row.get(0),
        )?;
        if !owns_collection {
            return Err(StoreError::CollectionWikiMismatch);
        }
        let mut statement = self.connection.prepare(
            "SELECT pages.page_id, pages.namespace, pages.current_title,
                    pages.current_revision_id
             FROM collection_pages
             JOIN pages USING (wiki_id, page_id)
             WHERE collection_pages.collection_id = ?1 AND pages.wiki_id = ?2
             ORDER BY pages.page_id",
        )?;
        let rows = statement
            .query_map(
                params![
                    to_sql_integer(collection_id.get())?,
                    to_sql_integer(wiki_id.get())?
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(raw_page_id, namespace, title, raw_revision_id)| {
                Ok(StoredPage {
                    wiki_id,
                    page_id: sql_id(raw_page_id, "invalid stored page ID")?,
                    namespace,
                    title: PageTitle::new(title)
                        .map_err(|_| StoreError::CorruptMetadata("invalid stored page title"))?,
                    current_revision_id: raw_revision_id
                        .map(|value| sql_id(value, "invalid stored revision ID"))
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Marks a selected page currently unavailable while retaining its captured head
    /// and complete local history.
    pub fn mark_page_missing(
        &mut self,
        wiki_id: WikiId,
        collection_id: CollectionId,
        page_id: PageId,
    ) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE pages SET state = 'missing', updated_at = ?4
             WHERE wiki_id = ?1 AND page_id = ?2
               AND EXISTS (
                   SELECT 1 FROM collection_pages
                   JOIN collections USING (collection_id)
                   WHERE collection_pages.collection_id = ?3
                     AND collection_pages.wiki_id = ?1
                     AND collection_pages.page_id = ?2
                     AND collections.wiki_id = ?1
               )",
            params![
                to_sql_integer(wiki_id.get())?,
                to_sql_integer(page_id.get())?,
                to_sql_integer(collection_id.get())?,
                unix_time()?,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::PageNotFound { wiki_id, page_id });
        }
        Ok(())
    }

    /// Records an observed page title and head after that revision is durable.
    ///
    /// The page must already belong to the collection and the revision must already
    /// be captured for that page. This transaction changes only mutable page-head
    /// metadata; immutable revision and content-object records are never rewritten.
    pub fn reconcile_current_revision(
        &mut self,
        wiki_id: WikiId,
        collection_id: CollectionId,
        page_id: PageId,
        namespace: i32,
        title: &PageTitle,
        revision_id: RevisionId,
    ) -> Result<(), StoreError> {
        let now = unix_time()?;
        let raw_wiki_id = to_sql_integer(wiki_id.get())?;
        let raw_collection_id = to_sql_integer(collection_id.get())?;
        let raw_page_id = to_sql_integer(page_id.get())?;
        let raw_revision_id = to_sql_integer(revision_id.get())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let selected: bool = transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM collection_pages
                JOIN collections USING (collection_id)
                WHERE collection_pages.collection_id = ?1
                  AND collection_pages.wiki_id = ?2
                  AND collection_pages.page_id = ?3
                  AND collections.wiki_id = ?2
             )",
            params![raw_collection_id, raw_wiki_id, raw_page_id],
            |row| row.get(0),
        )?;
        if !selected {
            return Err(StoreError::PageNotFound { wiki_id, page_id });
        }
        let revision_time: Option<String> = transaction
            .query_row(
                "SELECT revision_time FROM revisions
                 WHERE wiki_id = ?1 AND page_id = ?2 AND revision_id = ?3",
                params![raw_wiki_id, raw_page_id, raw_revision_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(revision_time) = revision_time else {
            return Err(StoreError::RevisionNotFound(revision_id));
        };

        transaction.execute(
            "UPDATE pages SET namespace = ?3, current_title = ?4,
                    current_revision_id = ?5, current_revision_time = ?6,
                    state = 'active', updated_at = ?7
             WHERE wiki_id = ?1 AND page_id = ?2",
            params![
                raw_wiki_id,
                raw_page_id,
                namespace,
                title.as_str(),
                raw_revision_id,
                revision_time,
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO page_titles (
                wiki_id, page_id, title, first_observed_at, last_observed_at
             ) VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(wiki_id, page_id, title) DO UPDATE SET
                last_observed_at = excluded.last_observed_at",
            params![raw_wiki_id, raw_page_id, title.as_str(), now],
        )?;
        let unresolved_removed = transaction.execute(
            "DELETE FROM unresolved_titles WHERE collection_id = ?1 AND title = ?2",
            params![raw_collection_id, title.as_str()],
        )?;
        if unresolved_removed != 0 {
            let changed = transaction.execute(
                "UPDATE collections SET generation = generation + 1
                 WHERE collection_id = ?1 AND status = 'active'",
                [raw_collection_id],
            )?;
            if changed != 1 {
                return Err(StoreError::CollectionTombstoned(collection_id));
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Finds pages with a remote ID across configured wikis.
    ///
    /// Remote page IDs are unique only within a wiki, so callers should report
    /// ambiguity instead of silently selecting the first match.
    pub fn pages_by_id(&self, page_id: PageId) -> Result<Vec<StoredPage>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT wiki_id, namespace, current_title, current_revision_id
             FROM pages WHERE page_id = ?1 ORDER BY wiki_id",
        )?;
        let rows = statement
            .query_map([to_sql_integer(page_id.get())?], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(raw_wiki_id, namespace, title, raw_revision_id)| {
                Ok(StoredPage {
                    wiki_id: sql_id(raw_wiki_id, "invalid stored wiki ID")?,
                    page_id,
                    namespace,
                    title: PageTitle::new(title)
                        .map_err(|_| StoreError::CorruptMetadata("invalid stored page title"))?,
                    current_revision_id: raw_revision_id
                        .map(|value| sql_id(value, "invalid stored revision ID"))
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Looks up captured pages by an observed title.
    ///
    /// When `wiki_id` is absent, matches from every configured wiki are returned so
    /// callers can report ambiguity instead of silently choosing one language.
    pub fn pages_by_title(
        &self,
        title: &PageTitle,
        wiki_id: Option<WikiId>,
    ) -> Result<Vec<StoredPage>, StoreError> {
        let wiki_filter = wiki_id.map(|id| to_sql_integer(id.get())).transpose()?;
        let mut statement = self.connection.prepare(
            "SELECT pages.wiki_id, pages.page_id, pages.namespace,
                    pages.current_title, pages.current_revision_id
             FROM page_titles AS titles
             JOIN pages USING (wiki_id, page_id)
             WHERE titles.title = ?1 AND (?2 IS NULL OR pages.wiki_id = ?2)
             ORDER BY pages.wiki_id, pages.page_id",
        )?;
        let rows = statement
            .query_map(params![title.as_str(), wiki_filter], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(raw_wiki_id, raw_page_id, namespace, current_title, raw_revision_id)| {
                    Ok(StoredPage {
                        wiki_id: sql_id(raw_wiki_id, "invalid stored wiki ID")?,
                        page_id: sql_id(raw_page_id, "invalid stored page ID")?,
                        namespace,
                        title: PageTitle::new(current_title).map_err(|_| {
                            StoreError::CorruptMetadata("invalid stored page title")
                        })?,
                        current_revision_id: raw_revision_id
                            .map(|value| sql_id(value, "invalid stored revision ID"))
                            .transpose()?,
                    })
                },
            )
            .collect()
    }

    /// Returns every title observed for a captured page in lexical order.
    pub fn page_titles(
        &self,
        wiki_id: WikiId,
        page_id: PageId,
    ) -> Result<Vec<PageTitle>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT title FROM page_titles
             WHERE wiki_id = ?1 AND page_id = ?2 ORDER BY title",
        )?;
        let titles = statement
            .query_map(
                params![
                    to_sql_integer(wiki_id.get())?,
                    to_sql_integer(page_id.get())?
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        titles
            .into_iter()
            .map(|title| {
                PageTitle::new(title)
                    .map_err(|_| StoreError::CorruptMetadata("invalid stored page title"))
            })
            .collect()
    }

    /// Looks up a captured revision and its logical content identity.
    pub fn revision(
        &self,
        wiki_id: WikiId,
        revision_id: RevisionId,
    ) -> Result<Option<StoredRevision>, StoreError> {
        self.connection
            .query_row(
                "SELECT wiki_id, revision_id, page_id, parent_revision_id,
                        revision_time, author_name, author_id, comment, is_minor,
                        source_size, upstream_sha1, content_model, content_object_id,
                        captured_at
                 FROM revisions WHERE wiki_id = ?1 AND revision_id = ?2",
                params![
                    to_sql_integer(wiki_id.get())?,
                    to_sql_integer(revision_id.get())?
                ],
                revision_row,
            )
            .optional()?
            .map(|row| stored_revision(row).map(|(_, revision)| revision))
            .transpose()
    }

    /// Lists every captured revision for a page, newest first.
    pub fn revisions_for_page(
        &self,
        wiki_id: WikiId,
        page_id: PageId,
    ) -> Result<Vec<StoredRevision>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT wiki_id, revision_id, page_id, parent_revision_id,
                    revision_time, author_name, author_id, comment, is_minor,
                    source_size, upstream_sha1, content_model, content_object_id,
                    captured_at
             FROM revisions
             WHERE wiki_id = ?1 AND page_id = ?2
             ORDER BY revision_time DESC, revision_id DESC",
        )?;
        let rows = statement
            .query_map(
                params![
                    to_sql_integer(wiki_id.get())?,
                    to_sql_integer(page_id.get())?
                ],
                revision_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| stored_revision(row).map(|(_, revision)| revision))
            .collect()
    }

    /// Returns the newest durable revision for a page without loading its history.
    pub fn newest_revision_for_page(
        &self,
        wiki_id: WikiId,
        page_id: PageId,
    ) -> Result<Option<StoredRevision>, StoreError> {
        self.connection
            .query_row(
                "SELECT wiki_id, revision_id, page_id, parent_revision_id,
                        revision_time, author_name, author_id, comment, is_minor,
                        source_size, upstream_sha1, content_model, content_object_id,
                        captured_at
                 FROM revisions
                 WHERE wiki_id = ?1 AND page_id = ?2
                 ORDER BY revision_time DESC, revision_id DESC LIMIT 1",
                params![
                    to_sql_integer(wiki_id.get())?,
                    to_sql_integer(page_id.get())?
                ],
                revision_row,
            )
            .optional()?
            .map(|row| stored_revision(row).map(|(_, revision)| revision))
            .transpose()
    }

    /// Returns the newest durable revision for a page at or before `cutoff`.
    ///
    /// `cutoff` must use MediaWiki's canonical UTC RFC 3339 representation,
    /// `YYYY-MM-DDTHH:MM:SSZ`. Captured revision timestamps use that same fixed-width
    /// representation, so SQLite's binary text comparison is chronological. Equal
    /// timestamps are resolved deterministically in favor of the larger revision ID.
    /// The page-time index and `LIMIT 1` keep this query from materializing history.
    pub fn newest_revision_for_page_at_or_before(
        &self,
        wiki_id: WikiId,
        page_id: PageId,
        cutoff: &str,
    ) -> Result<Option<StoredRevision>, StoreError> {
        validate_mediawiki_timestamp(cutoff)?;
        self.connection
            .query_row(
                "SELECT wiki_id, revision_id, page_id, parent_revision_id,
                        revision_time, author_name, author_id, comment, is_minor,
                        source_size, upstream_sha1, content_model, content_object_id,
                        captured_at
                 FROM revisions
                 WHERE wiki_id = ?1 AND page_id = ?2 AND revision_time <= ?3
                 ORDER BY revision_time DESC, revision_id DESC LIMIT 1",
                params![
                    to_sql_integer(wiki_id.get())?,
                    to_sql_integer(page_id.get())?,
                    cutoff
                ],
                revision_row,
            )
            .optional()?
            .map(|row| stored_revision(row).map(|(_, revision)| revision))
            .transpose()
    }

    /// Finds revisions with a remote ID across configured wikis.
    ///
    /// Remote revision IDs are unique only within a wiki, so callers should report
    /// ambiguity instead of silently selecting the first match.
    pub fn revisions_by_id(
        &self,
        revision_id: RevisionId,
    ) -> Result<Vec<(WikiId, StoredRevision)>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT wiki_id, revision_id, page_id, parent_revision_id,
                    revision_time, author_name, author_id, comment, is_minor,
                    source_size, upstream_sha1, content_model, content_object_id,
                    captured_at
             FROM revisions WHERE revision_id = ?1 ORDER BY wiki_id",
        )?;
        let rows = statement
            .query_map([to_sql_integer(revision_id.get())?], revision_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(stored_revision).collect()
    }

    /// Lists the most recently captured source revisions across all configured wikis.
    pub fn recent_revisions(
        &self,
        limit: u32,
    ) -> Result<Vec<(WikiId, StoredRevision)>, StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "recent revision limit must be between 1 and 100",
            ));
        }
        let mut statement = self.connection.prepare(
            "SELECT wiki_id, revision_id, page_id, parent_revision_id,
                    revision_time, author_name, author_id, comment, is_minor,
                    source_size, upstream_sha1, content_model, content_object_id,
                    captured_at
             FROM revisions
             ORDER BY revision_time DESC, wiki_id, revision_id DESC
             LIMIT ?1",
        )?;
        let rows = statement
            .query_map([i64::from(limit)], revision_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(stored_revision).collect()
    }

    /// Looks up one collection, including a tombstoned collection retained for audit.
    pub fn collection(
        &self,
        collection_id: CollectionId,
    ) -> Result<Option<StoredCollection>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT collections.wiki_id, collections.name, collections.generation,
                        collections.status, collections.tombstoned_at,
                        COUNT(members.page_id)
                 FROM collections
                 LEFT JOIN collection_resolved_members AS members
                   ON members.collection_id = collections.collection_id
                  AND members.membership_state = 'active'
                 WHERE collections.collection_id = ?1
                 GROUP BY collections.collection_id",
                [to_sql_integer(collection_id.get())?],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(wiki_id, name, generation, status, tombstoned_at, page_count)| {
                stored_collection(
                    collection_id.get(),
                    wiki_id,
                    name,
                    generation,
                    status,
                    tombstoned_at,
                    page_count,
                )
            },
        )
        .transpose()
    }

    /// Lists active collection summaries in deterministic name and identity order.
    pub fn collections(&self) -> Result<Vec<StoredCollection>, StoreError> {
        self.collection_summaries(false)
    }

    /// Lists active and tombstoned collections for audit and administrative views.
    pub fn collections_including_tombstones(&self) -> Result<Vec<StoredCollection>, StoreError> {
        self.collection_summaries(true)
    }

    fn collection_summaries(
        &self,
        include_tombstones: bool,
    ) -> Result<Vec<StoredCollection>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT collections.collection_id, collections.wiki_id, collections.name,
                    collections.generation, collections.status, collections.tombstoned_at,
                    COUNT(members.page_id)
             FROM collections
             LEFT JOIN collection_resolved_members AS members
               ON members.collection_id = collections.collection_id
              AND members.membership_state = 'active'
             WHERE (?1 OR collections.status = 'active')
             GROUP BY collections.collection_id
             ORDER BY collections.name, collections.collection_id",
        )?;
        let rows = statement
            .query_map([include_tombstones], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(collection_id, wiki_id, name, generation, status, tombstoned_at, page_count)| {
                    stored_collection(
                        sql_u64(collection_id, "invalid collection ID")?,
                        wiki_id,
                        name,
                        generation,
                        status,
                        tombstoned_at,
                        page_count,
                    )
                },
            )
            .collect()
    }

    /// Stores an in-memory canonical object.
    pub fn put_bytes(
        &mut self,
        kind: ObjectKind,
        bytes: &[u8],
    ) -> Result<StoredObject, StoreError> {
        let length = u64::try_from(bytes.len()).expect("slice length fits in u64");
        self.put_reader(kind, length, bytes)
    }

    /// Streams exactly `expected_length` canonical bytes into a durable loose object.
    ///
    /// No SQLite row is created if the reader fails or returns a different length.
    /// The expected length and configured maximum prevent an unbounded decompression
    /// or source stream from filling the library.
    pub fn put_reader(
        &mut self,
        kind: ObjectKind,
        expected_length: u64,
        mut reader: impl Read,
    ) -> Result<StoredObject, StoreError> {
        self.ensure_writable()?;
        if expected_length > self.config.max_object_bytes {
            return Err(StoreError::ObjectTooLarge {
                limit: self.config.max_object_bytes,
                actual: expected_length,
            });
        }

        let mut temporary = tempfile::Builder::new()
            .prefix("object-")
            .suffix(".tmp")
            .tempfile_in(self.root.join("tmp"))?;
        let mut hasher = object_hasher(kind, expected_length);
        let mut encoder = zstd::stream::write::Encoder::new(
            temporary.as_file_mut(),
            self.config.compression_level,
        )?;
        let mut buffer = [0_u8; COPY_BUFFER_SIZE];
        let mut actual_length = 0_u64;

        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            actual_length =
                actual_length
                    .checked_add(count as u64)
                    .ok_or(StoreError::ObjectTooLarge {
                        limit: self.config.max_object_bytes,
                        actual: u64::MAX,
                    })?;
            if actual_length > expected_length || actual_length > self.config.max_object_bytes {
                return Err(StoreError::LengthMismatch {
                    expected: expected_length,
                    actual: actual_length,
                });
            }
            hasher.update(&buffer[..count]);
            encoder.write_all(&buffer[..count])?;
        }
        encoder.finish()?;

        if actual_length != expected_length {
            return Err(StoreError::LengthMismatch {
                expected: expected_length,
                actual: actual_length,
            });
        }

        temporary.as_file().sync_all()?;
        let compressed_length = temporary.as_file().metadata()?.len();
        let id = ObjectId(*hasher.finalize().as_bytes());
        let relative_path = loose_relative_path(id);
        let absolute_path = self.root.join(&relative_path);
        let parent = absolute_path.parent().ok_or(StoreError::CorruptMetadata(
            "loose object path has no parent",
        ))?;
        create_private_dir_all(parent)?;

        // Replacing an existing path is safe: the content-derived target name can
        // only be reached by the same canonical bytes. `persist` uses an atomic
        // rename on the supported macOS and Linux filesystems, so readers observe
        // either complete representation rather than a partially copied file.
        temporary
            .persist(&absolute_path)
            .map_err(|error| StoreError::Io(error.error))?;
        sync_directory(parent)?;

        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO content_objects (
                object_id, object_kind, uncompressed_length, media_type,
                verification_state, created_at
             ) VALUES (?1, ?2, ?3, ?4, 'verified', ?5)",
            params![
                id.to_string(),
                kind.database_value(),
                to_sql_integer(expected_length)?,
                kind.default_media_type(),
                now,
            ],
        )?;

        let existing: (String, i64) = transaction.query_row(
            "SELECT object_kind, uncompressed_length
             FROM content_objects WHERE object_id = ?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if existing.0 != kind.database_value() || existing.1 != to_sql_integer(expected_length)? {
            return Err(StoreError::CorruptMetadata(
                "object ID is associated with conflicting metadata",
            ));
        }

        transaction.execute(
            "INSERT OR IGNORE INTO object_locations (
                object_id, storage_kind, encoding, relative_path, compressed_length,
                verification_state, created_at
             ) VALUES (?1, 'loose', 'zstd', ?2, ?3, 'verified', ?4)",
            params![
                id.to_string(),
                path_to_database(&relative_path)?,
                to_sql_integer(compressed_length)?,
                now,
            ],
        )?;
        transaction.commit()?;

        Ok(StoredObject {
            id,
            kind,
            uncompressed_length: expected_length,
        })
    }

    /// Builds and activates one bounded immutable pack from verified loose objects.
    ///
    /// The pack and its separate index are made durable and every entry is
    /// reconstructed and hash-verified before one SQLite transaction exposes any
    /// packed location. Candidates are ordered deterministically by object kind,
    /// captured page affinity, and logarithmic size class before bounded delta
    /// selection. A loose object larger than the per-pack input limit remains loose
    /// without blocking smaller eligible objects. Existing loose representations
    /// remain available until an explicit pruning pass marks them obsolete.
    pub fn pack_loose_objects(&mut self) -> Result<Option<PackSummary>, StoreError> {
        self.ensure_writable()?;
        let candidates = {
            let mut statement = self.connection.prepare(
                "WITH candidates AS (
                    SELECT loose.object_id, MIN(loose.location_id) AS stable_order
                    FROM object_locations AS loose
                    JOIN content_objects AS objects USING (object_id)
                    WHERE loose.storage_kind = 'loose'
                      AND loose.encoding = 'zstd'
                      AND loose.verification_state = 'verified'
                      AND objects.uncompressed_length <= ?2
                      AND NOT EXISTS (
                          SELECT 1 FROM object_locations AS packed
                          JOIN packs ON packs.pack_id = packed.pack_id
                          WHERE packed.object_id = loose.object_id
                            AND packed.storage_kind = 'pack'
                            AND packed.verification_state = 'verified'
                            AND packs.state = 'verified'
                      )
                    GROUP BY loose.object_id
                    ORDER BY stable_order
                    LIMIT ?1
                 )
                 SELECT candidates.object_id, candidates.stable_order,
                        affinity.wiki_id, affinity.page_id, affinity.revision_id
                 FROM candidates
                 LEFT JOIN revisions AS affinity ON affinity.rowid = (
                     SELECT revision.rowid
                     FROM revisions AS revision
                     WHERE revision.content_object_id = candidates.object_id
                     ORDER BY revision.wiki_id, revision.page_id, revision.revision_id
                     LIMIT 1
                 )
                 ORDER BY candidates.stable_order",
            )?;
            statement
                .query_map(
                    params![
                        i64::from(self.config.max_pack_objects),
                        self.config.max_pack_input_bytes.min(i64::MAX as u64) as i64,
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?
        };

        let mut sources = Vec::with_capacity(candidates.len());
        for (raw_id, stable_order, raw_wiki_id, raw_page_id, raw_revision_id) in candidates {
            let id = raw_id
                .parse::<ObjectId>()
                .map_err(|_| StoreError::CorruptMetadata("invalid stored object ID"))?;
            let (kind, _, bytes) = self.read_loose_object(id)?;
            let (affinity, revision_order) =
                pack_affinity(raw_wiki_id, raw_page_id, raw_revision_id)?;
            sources.push(PackSource {
                id,
                kind,
                bytes,
                affinity,
                revision_order,
                stable_order: sql_u64(stable_order, "invalid loose location order")?,
            });
        }
        if sources.is_empty() {
            return Ok(None);
        }
        sources.sort_by_key(PackSource::sort_key);
        let mut total_input = 0_u64;
        sources.retain(|source| {
            let Some(next_total) = total_input.checked_add(source.bytes.len() as u64) else {
                return false;
            };
            if next_total > self.config.max_pack_input_bytes {
                false
            } else {
                total_input = next_total;
                true
            }
        });
        if sources.is_empty() {
            return Ok(None);
        }

        self.activate_pack_sources(&sources).map(Some)
    }

    /// Rewrites one verified pack's objects into a freshly tuned generation.
    /// The previous representation remains active until [`Self::retire_pack`] is
    /// called, so interruption cannot strand an object between generations.
    pub fn repack_pack(&mut self, pack_id: &str) -> Result<PackSummary, StoreError> {
        self.ensure_writable()?;
        let recorded = self.verify_recorded_pack(pack_id)?;
        if recorded.object_count > u64::from(self.config.max_pack_objects) {
            return Err(StoreError::PackLimitExceeded);
        }
        let candidates = {
            let mut statement = self.connection.prepare(
                "SELECT locations.object_id, locations.pack_offset,
                        affinity.wiki_id, affinity.page_id, affinity.revision_id
                 FROM object_locations AS locations
                 LEFT JOIN revisions AS affinity ON affinity.rowid = (
                     SELECT revision.rowid
                     FROM revisions AS revision
                     WHERE revision.content_object_id = locations.object_id
                     ORDER BY revision.wiki_id, revision.page_id, revision.revision_id
                     LIMIT 1
                 )
                 WHERE locations.pack_id = ?1
                   AND locations.verification_state = 'verified'
                 ORDER BY locations.pack_offset",
            )?;
            statement
                .query_map([pack_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut sources = Vec::with_capacity(candidates.len());
        let mut total_input = 0_u64;
        for (raw_id, stable_order, raw_wiki_id, raw_page_id, raw_revision_id) in candidates {
            let id = raw_id
                .parse::<ObjectId>()
                .map_err(|_| StoreError::CorruptMetadata("invalid stored object ID"))?;
            let (kind, expected_length, _) = self.object_locations(id)?;
            let next_total = total_input
                .checked_add(expected_length)
                .ok_or(StoreError::PackLimitExceeded)?;
            if next_total > self.config.max_pack_input_bytes {
                return Err(StoreError::PackLimitExceeded);
            }
            let bytes = self.read_object(id)?;
            let (affinity, revision_order) =
                pack_affinity(raw_wiki_id, raw_page_id, raw_revision_id)?;
            total_input = next_total;
            sources.push(PackSource {
                id,
                kind,
                bytes,
                affinity,
                revision_order,
                stable_order: sql_u64(stable_order, "invalid pack offset")?,
            });
        }
        if sources.len() as u64 != recorded.object_count {
            return Err(StoreError::CorruptMetadata(
                "pack object count disagrees with locations",
            ));
        }
        sources.sort_by_key(PackSource::sort_key);
        self.activate_pack_sources(&sources)
    }

    fn activate_pack_sources(&mut self, sources: &[PackSource]) -> Result<PackSummary, StoreError> {
        self.ensure_writable()?;
        let raw_generation: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(generation), 0) + 1 FROM packs",
            [],
            |row| row.get(0),
        )?;
        let generation = sql_u64(raw_generation, "invalid pack generation")?;
        let mut entries = prepare_pack_entries(sources, self.config.compression_level)?;

        let mut pack_temp = tempfile::Builder::new()
            .prefix("pack-")
            .suffix(".tmp")
            .tempfile_in(self.root.join("tmp"))?;
        write_pack(pack_temp.as_file_mut(), generation, &mut entries)?;
        pack_temp.as_file().sync_all()?;
        let pack_checksum = checksum_file(pack_temp.as_file_mut())?;
        let pack_bytes = pack_temp.as_file().metadata()?.len();

        let mut index_temp = tempfile::Builder::new()
            .prefix("index-")
            .suffix(".tmp")
            .tempfile_in(self.root.join("tmp"))?;
        write_pack_index(index_temp.as_file_mut(), pack_checksum, &entries)?;
        index_temp.as_file().sync_all()?;
        let index_checksum = checksum_file(index_temp.as_file_mut())?;
        let index_bytes = index_temp.as_file().metadata()?.len();

        let pack_digest = blake3::Hash::from_bytes(pack_checksum).to_hex().to_string();
        let index_digest = blake3::Hash::from_bytes(index_checksum)
            .to_hex()
            .to_string();
        let pack_id = format!("b3:{pack_digest}");
        let pack_relative = PathBuf::from("objects/packs").join(format!("pack-{pack_digest}.pack"));
        let index_relative = PathBuf::from("objects/packs").join(format!("pack-{pack_digest}.idx"));
        let pack_absolute = self.root.join(&pack_relative);
        let index_absolute = self.root.join(&index_relative);
        pack_temp
            .persist(&pack_absolute)
            .map_err(|error| StoreError::Io(error.error))?;
        index_temp
            .persist(&index_absolute)
            .map_err(|error| StoreError::Io(error.error))?;
        sync_directory(&self.root.join("objects/packs"))?;

        verify_pack_files(
            &pack_absolute,
            &index_absolute,
            pack_checksum,
            index_checksum,
            generation,
            self.config.max_object_bytes,
            entries.len() as u64,
        )?;

        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO packs (
                pack_id, generation, pack_path, index_path, pack_checksum,
                index_checksum, object_count, state, created_at, verified_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'verified', ?8, ?8)",
            params![
                pack_id,
                to_sql_integer(generation)?,
                path_to_database(&pack_relative)?,
                path_to_database(&index_relative)?,
                format!("b3:{pack_digest}"),
                format!("b3:{index_digest}"),
                to_sql_integer(entries.len() as u64)?,
                now,
            ],
        )?;
        for entry in &entries {
            transaction.execute(
                "INSERT INTO object_locations (
                    object_id, storage_kind, encoding, relative_path,
                    compressed_length, base_object_id, pack_generation,
                    verification_state, created_at, pack_id, pack_offset, delta_depth
                 ) VALUES (?1, 'pack', ?2, ?3, ?4, ?5, ?6, 'verified', ?7, ?8, ?9, ?10)",
                params![
                    entry.id.to_string(),
                    entry.encoding.database_value(),
                    path_to_database(&pack_relative)?,
                    to_sql_integer(entry.record_length)?,
                    entry.base_id.map(|id| id.to_string()),
                    to_sql_integer(generation)?,
                    now,
                    pack_id,
                    to_sql_integer(entry.offset)?,
                    i64::from(entry.delta_depth),
                ],
            )?;
        }
        transaction.commit()?;

        let full_entries = entries
            .iter()
            .filter(|entry| entry.encoding == PackEncoding::Full)
            .count() as u64;
        let delta_entries = entries.len() as u64 - full_entries;
        Ok(PackSummary {
            pack_id,
            generation,
            object_count: entries.len() as u64,
            full_entries,
            delta_entries,
            pack_bytes,
            index_bytes,
        })
    }

    fn recorded_pack(&self, pack_id: &str) -> Result<RecordedPack, StoreError> {
        let metadata: Option<(String, String, String, String, i64, i64)> = self
            .connection
            .query_row(
                "SELECT pack_path, index_path, pack_checksum, index_checksum,
                        generation, object_count
                 FROM packs WHERE pack_id = ?1 AND state = 'verified'",
                [pack_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let (pack_path, index_path, pack_checksum, index_checksum, generation, object_count) =
            metadata.ok_or_else(|| StoreError::PackNotFound(pack_id.to_owned()))?;
        Ok(RecordedPack {
            pack_path: pack_database_path(&pack_path, ".pack")?,
            index_path: pack_database_path(&index_path, ".idx")?,
            pack_checksum: parse_checksum(&pack_checksum)?,
            index_checksum: parse_checksum(&index_checksum)?,
            generation: sql_u64(generation, "invalid pack generation")?,
            object_count: sql_u64(object_count, "invalid pack object count")?,
        })
    }

    fn verify_recorded_pack(&self, pack_id: &str) -> Result<RecordedPack, StoreError> {
        let pack = self.recorded_pack(pack_id)?;
        verify_pack_files(
            &self.root.join(&pack.pack_path),
            &self.root.join(&pack.index_path),
            pack.pack_checksum,
            pack.index_checksum,
            pack.generation,
            self.config.max_object_bytes,
            pack.object_count,
        )?;
        Ok(pack)
    }

    /// Marks loose copies represented by a verified pack obsolete, then removes them.
    ///
    /// The metadata transition commits first, so interruption can leave only harmless
    /// orphaned loose files and never remove the active packed representation.
    pub fn prune_packed_loose_objects(&mut self, pack_id: &str) -> Result<u64, StoreError> {
        self.ensure_writable()?;
        self.verify_recorded_pack(pack_id)?;
        let paths = {
            let mut statement = self.connection.prepare(
                "SELECT DISTINCT loose.relative_path
                 FROM object_locations AS loose
                 JOIN object_locations AS packed ON packed.object_id = loose.object_id
                 WHERE packed.pack_id = ?1
                   AND packed.verification_state = 'verified'
                   AND loose.storage_kind = 'loose'
                   AND loose.verification_state = 'verified'",
            )?;
            statement
                .query_map([pack_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE object_locations AS loose
             SET verification_state = 'obsolete'
             WHERE loose.storage_kind = 'loose'
               AND loose.verification_state = 'verified'
               AND EXISTS (
                   SELECT 1 FROM object_locations AS packed
                   WHERE packed.pack_id = ?1
                     AND packed.object_id = loose.object_id
                     AND packed.verification_state = 'verified'
               )",
            [pack_id],
        )?;
        transaction.commit()?;
        for raw_path in paths {
            let path = loose_database_path(&raw_path)?;
            match fs::remove_file(self.root.join(path)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(StoreError::Io(error)),
            }
        }
        Ok(changed as u64)
    }

    /// Retires and removes one pack after verifying another representation of every
    /// object. This is the final, separately crash-safe phase of repacking.
    pub fn retire_pack(&mut self, pack_id: &str) -> Result<u64, StoreError> {
        self.ensure_writable()?;
        let retiring = self.recorded_pack(pack_id)?;
        let object_ids = {
            let mut statement = self.connection.prepare(
                "SELECT object_id FROM object_locations
                 WHERE pack_id = ?1 AND verification_state = 'verified'
                 ORDER BY object_id",
            )?;
            statement
                .query_map([pack_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if object_ids.len() as u64 != retiring.object_count {
            return Err(StoreError::CorruptMetadata(
                "pack object count disagrees with locations",
            ));
        }
        for raw_id in &object_ids {
            let id = raw_id
                .parse::<ObjectId>()
                .map_err(|_| StoreError::CorruptMetadata("invalid stored object ID"))?;
            let (kind, expected_length, locations) = self.object_locations(id)?;
            let mut verified_alternative = false;
            for location in locations {
                if location.pack_id.as_deref() == Some(pack_id) {
                    continue;
                }
                let result = if location.storage_kind == "loose" {
                    self.read_loose_location(id, kind, expected_length, &location.relative_path)
                } else {
                    self.read_pack_location(id, kind, expected_length, &location)
                };
                if result.is_ok() {
                    verified_alternative = true;
                    break;
                }
            }
            if !verified_alternative {
                return Err(StoreError::PackStillRequired(pack_id.to_owned()));
            }
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE object_locations SET verification_state = 'obsolete'
             WHERE pack_id = ?1 AND verification_state = 'verified'",
            [pack_id],
        )?;
        transaction.execute(
            "UPDATE packs SET state = 'obsolete' WHERE pack_id = ?1 AND state = 'verified'",
            [pack_id],
        )?;
        transaction.commit()?;
        for relative in [&retiring.pack_path, &retiring.index_path] {
            match fs::remove_file(self.root.join(relative)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(StoreError::Io(error)),
            }
        }
        sync_directory(&self.root.join("objects/packs"))?;
        Ok(changed as u64)
    }

    /// Returns whether verified metadata and a physical location are recorded.
    pub fn contains(&self, id: ObjectId) -> Result<bool, StoreError> {
        let exists = self.connection.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM content_objects AS objects
                JOIN object_locations AS locations USING (object_id)
                WHERE objects.object_id = ?1
                  AND objects.verification_state = 'verified'
                  AND locations.verification_state = 'verified'
                  AND (
                      locations.storage_kind = 'loose'
                      OR EXISTS (
                          SELECT 1 FROM packs
                          WHERE packs.pack_id = locations.pack_id
                            AND packs.state = 'verified'
                      )
                  )
             )",
            [id.to_string()],
            |row| row.get(0),
        )?;
        Ok(exists)
    }

    /// Returns the number of logical canonical objects known to the library.
    pub fn logical_object_count(&self) -> Result<u64, StoreError> {
        let count: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM content_objects", [], |row| row.get(0))?;
        sql_u64(count, "negative logical object count")
    }

    /// Enumerates logical objects after an optional cursor in strict ID order.
    ///
    /// Callers performing full verification should pass the final ID from one page
    /// into the next call. The page size is bounded to `1..=1000` so GUI background
    /// work cannot accidentally materialize an unbounded library inventory.
    pub fn logical_objects_after(
        &self,
        after: Option<ObjectId>,
        limit: u32,
    ) -> Result<Vec<LogicalObject>, StoreError> {
        if !(1..=1_000).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "logical object page size must be between 1 and 1000",
            ));
        }
        let after = after.map(|id| id.to_string()).unwrap_or_default();
        let mut statement = self.connection.prepare(
            "SELECT object_id, object_kind, uncompressed_length,
                    media_type, verification_state
             FROM content_objects
             WHERE object_id > ?1
             ORDER BY object_id
             LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![after, i64::from(limit)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, kind, length, media_type, verification_state)| {
                Ok(LogicalObject {
                    object: StoredObject {
                        id: id
                            .parse()
                            .map_err(|_| StoreError::CorruptMetadata("invalid stored object ID"))?,
                        kind: ObjectKind::from_database(&kind)?,
                        uncompressed_length: sql_u64(length, "negative object length")?,
                    },
                    media_type,
                    verification_state: ObjectVerificationState::from_database(
                        &verification_state,
                    )?,
                })
            })
            .collect()
    }

    /// Returns the number of records covered by the current full metadata-integrity
    /// scan, including canonical media metadata and revision media placements.
    pub fn integrity_metadata_record_count(&self) -> Result<u64, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT
                (SELECT COUNT(*) FROM revisions)
              + (SELECT COUNT(*) FROM pages)
              + (SELECT COUNT(*) FROM sync_checkpoints)
              + (SELECT COUNT(*) FROM search_documents)
              + (SELECT COUNT(*) FROM search_fts)
              + (SELECT COUNT(*) FROM media)
              + (SELECT COUNT(*) FROM page_media)",
            [],
            |row| row.get(0),
        )?;
        sql_u64(count, "negative integrity metadata record count")
    }

    /// Returns SQLite's connection-local change counter used to detect commits from
    /// another connection during a multi-page metadata scan.
    pub fn integrity_metadata_change_counter(&self) -> Result<u64, StoreError> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))?;
        sql_u64(version, "negative SQLite data version")
    }

    /// Enumerates current-schema metadata references in stable kind/key order.
    ///
    /// Parent revisions that are not captured are valid and are not reported. If a
    /// parent is present locally, it must belong to the same page. Search text is
    /// rebuildable, so this validates pointers but does not treat an absent search
    /// document for a page as canonical-content loss.
    pub fn integrity_metadata_records_after(
        &self,
        after: Option<IntegrityMetadataCursor>,
        limit: u32,
    ) -> Result<Vec<IntegrityMetadataRecord>, StoreError> {
        if !(1..=MAX_INTEGRITY_METADATA_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "integrity metadata page size must be between 1 and 1,000",
            ));
        }
        let mut category = after.map_or(0, |cursor| cursor.category);
        let mut first_key = after.map_or(-1, |cursor| cursor.first_key);
        let mut second_key = after.map_or(-1, |cursor| cursor.second_key);
        let mut records = Vec::with_capacity(limit as usize);

        while records.len() < limit as usize && category <= 6 {
            let remaining = i64::from(limit) - records.len() as i64;
            let before = records.len();
            match category {
                0 => {
                    let mut statement = self.connection.prepare(
                        "SELECT revision.wiki_id, revision.revision_id,
                                EXISTS (
                                    SELECT 1 FROM pages
                                    WHERE pages.wiki_id = revision.wiki_id
                                      AND pages.page_id = revision.page_id
                                ),
                                EXISTS (
                                    SELECT 1 FROM content_objects
                                    WHERE content_objects.object_id = revision.content_object_id
                                ),
                                revision.parent_revision_id IS NULL OR NOT EXISTS (
                                    SELECT 1 FROM revisions AS parent
                                    WHERE parent.wiki_id = revision.wiki_id
                                      AND parent.revision_id = revision.parent_revision_id
                                ) OR EXISTS (
                                    SELECT 1 FROM revisions AS parent
                                    WHERE parent.wiki_id = revision.wiki_id
                                      AND parent.revision_id = revision.parent_revision_id
                                      AND parent.page_id = revision.page_id
                                ),
                                revision.parent_revision_id IS NOT revision.revision_id
                         FROM revisions AS revision
                         WHERE revision.wiki_id > ?1
                            OR (revision.wiki_id = ?1 AND revision.revision_id > ?2)
                         ORDER BY revision.wiki_id, revision.revision_id LIMIT ?3",
                    )?;
                    let rows =
                        statement.query_map(params![first_key, second_key, remaining], |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, bool>(2)?,
                                row.get::<_, bool>(3)?,
                                row.get::<_, bool>(4)?,
                                row.get::<_, bool>(5)?,
                            ))
                        })?;
                    for row in rows {
                        let (wiki_id, revision_id, page, object, parent_page, parent_self) = row?;
                        let mut issues = Vec::new();
                        if !page {
                            issues.push(IntegrityMetadataIssue::RevisionPageMissing);
                        }
                        if !object {
                            issues.push(IntegrityMetadataIssue::RevisionObjectMissing);
                        }
                        if !parent_page {
                            issues.push(IntegrityMetadataIssue::RevisionParentWrongPage);
                        }
                        if !parent_self {
                            issues.push(IntegrityMetadataIssue::RevisionParentSelfReference);
                        }
                        records.push(IntegrityMetadataRecord {
                            subject: IntegrityMetadataSubject::Revision {
                                wiki_id: sql_u64(wiki_id, "invalid revision wiki ID")?,
                                revision_id: sql_u64(revision_id, "invalid revision ID")?,
                            },
                            issues,
                            search_transformer_version: None,
                            media_object: None,
                        });
                    }
                }
                1 => {
                    let mut statement = self.connection.prepare(
                        "SELECT page.wiki_id, page.page_id,
                                page.current_revision_id IS NULL OR EXISTS (
                                    SELECT 1 FROM revisions AS revision
                                    WHERE revision.wiki_id = page.wiki_id
                                      AND revision.revision_id = page.current_revision_id
                                ),
                                page.current_revision_id IS NULL OR EXISTS (
                                    SELECT 1 FROM revisions AS revision
                                    WHERE revision.wiki_id = page.wiki_id
                                      AND revision.revision_id = page.current_revision_id
                                      AND revision.page_id = page.page_id
                                )
                         FROM pages AS page
                         WHERE page.wiki_id > ?1
                            OR (page.wiki_id = ?1 AND page.page_id > ?2)
                         ORDER BY page.wiki_id, page.page_id LIMIT ?3",
                    )?;
                    let rows =
                        statement.query_map(params![first_key, second_key, remaining], |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, bool>(2)?,
                                row.get::<_, bool>(3)?,
                            ))
                        })?;
                    for row in rows {
                        let (wiki_id, page_id, revision, revision_page) = row?;
                        let mut issues = Vec::new();
                        if !revision {
                            issues.push(IntegrityMetadataIssue::PageHeadRevisionMissing);
                        } else if !revision_page {
                            issues.push(IntegrityMetadataIssue::PageHeadRevisionWrongPage);
                        }
                        records.push(IntegrityMetadataRecord {
                            subject: IntegrityMetadataSubject::Page {
                                wiki_id: sql_u64(wiki_id, "invalid page wiki ID")?,
                                page_id: sql_u64(page_id, "invalid page ID")?,
                            },
                            issues,
                            search_transformer_version: None,
                            media_object: None,
                        });
                    }
                }
                2 => {
                    let mut statement = self.connection.prepare(
                        "SELECT checkpoint.checkpoint_id,
                                checkpoint.collection_id IS NULL OR EXISTS (
                                    SELECT 1 FROM collections
                                    WHERE collections.collection_id = checkpoint.collection_id
                                      AND collections.wiki_id = checkpoint.wiki_id
                                ),
                                (checkpoint.last_run_id IS NULL
                                    AND checkpoint.committed_through = 0) OR EXISTS (
                                    SELECT 1 FROM sync_runs AS run
                                    WHERE run.run_id = checkpoint.last_run_id
                                ),
                                checkpoint.last_run_id IS NULL OR EXISTS (
                                    SELECT 1 FROM sync_runs AS run
                                    WHERE run.run_id = checkpoint.last_run_id
                                      AND run.state = 'succeeded'
                                ),
                                checkpoint.last_run_id IS NULL OR EXISTS (
                                    SELECT 1 FROM sync_runs AS run
                                    WHERE run.run_id = checkpoint.last_run_id
                                      AND run.wiki_id = checkpoint.wiki_id
                                      AND run.collection_id IS checkpoint.collection_id
                                ),
                                checkpoint.last_run_id IS NULL OR EXISTS (
                                    SELECT 1 FROM sync_runs AS run
                                    WHERE run.run_id = checkpoint.last_run_id
                                      AND run.checkpoint_candidate = checkpoint.committed_through
                                )
                         FROM sync_checkpoints AS checkpoint
                         WHERE checkpoint.checkpoint_id > ?1
                         ORDER BY checkpoint.checkpoint_id LIMIT ?2",
                    )?;
                    let rows = statement.query_map(params![first_key, remaining], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, bool>(2)?,
                            row.get::<_, bool>(3)?,
                            row.get::<_, bool>(4)?,
                            row.get::<_, bool>(5)?,
                        ))
                    })?;
                    for row in rows {
                        let (id, collection, run, succeeded, scope, boundary) = row?;
                        let mut issues = Vec::new();
                        if !collection {
                            issues.push(IntegrityMetadataIssue::CheckpointCollectionWikiMismatch);
                        }
                        if !run {
                            issues.push(IntegrityMetadataIssue::CheckpointRunMissing);
                        } else {
                            if !succeeded {
                                issues.push(IntegrityMetadataIssue::CheckpointRunNotSucceeded);
                            }
                            if !scope {
                                issues.push(IntegrityMetadataIssue::CheckpointRunScopeMismatch);
                            }
                            if !boundary {
                                issues.push(IntegrityMetadataIssue::CheckpointBoundaryMismatch);
                            }
                        }
                        records.push(IntegrityMetadataRecord {
                            subject: IntegrityMetadataSubject::Checkpoint {
                                checkpoint_id: sql_u64(id, "invalid checkpoint ID")?,
                            },
                            issues,
                            search_transformer_version: None,
                            media_object: None,
                        });
                    }
                }
                3 => {
                    let mut statement = self.connection.prepare(
                        "SELECT document.search_id, document.transformer_version,
                                EXISTS (
                                    SELECT 1 FROM pages AS page
                                    WHERE page.wiki_id = document.wiki_id
                                      AND page.page_id = document.page_id
                                ),
                                EXISTS (
                                    SELECT 1 FROM revisions AS revision
                                    WHERE revision.wiki_id = document.wiki_id
                                      AND revision.revision_id = document.revision_id
                                ),
                                EXISTS (
                                    SELECT 1 FROM revisions AS revision
                                    WHERE revision.wiki_id = document.wiki_id
                                      AND revision.revision_id = document.revision_id
                                      AND revision.page_id = document.page_id
                                ),
                                EXISTS (
                                    SELECT 1 FROM pages AS page
                                    WHERE page.wiki_id = document.wiki_id
                                      AND page.page_id = document.page_id
                                      AND page.current_revision_id = document.revision_id
                                ),
                                EXISTS (
                                    SELECT 1 FROM search_fts
                                    WHERE search_fts.rowid = document.search_id
                                )
                         FROM search_documents AS document
                         WHERE document.search_id > ?1
                         ORDER BY document.search_id LIMIT ?2",
                    )?;
                    let rows = statement.query_map(params![first_key, remaining], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, bool>(2)?,
                            row.get::<_, bool>(3)?,
                            row.get::<_, bool>(4)?,
                            row.get::<_, bool>(5)?,
                            row.get::<_, bool>(6)?,
                        ))
                    })?;
                    for row in rows {
                        let (id, version, page, revision, revision_page, current, fts) = row?;
                        let mut issues = Vec::new();
                        if !page {
                            issues.push(IntegrityMetadataIssue::SearchPageMissing);
                        }
                        if !revision {
                            issues.push(IntegrityMetadataIssue::SearchRevisionMissing);
                        } else if !revision_page {
                            issues.push(IntegrityMetadataIssue::SearchRevisionWrongPage);
                        }
                        if page && revision && revision_page && !current {
                            issues.push(IntegrityMetadataIssue::SearchRevisionNotCurrent);
                        }
                        if !fts {
                            issues.push(IntegrityMetadataIssue::SearchFtsRowMissing);
                        }
                        records.push(IntegrityMetadataRecord {
                            subject: IntegrityMetadataSubject::SearchDocument {
                                search_id: sql_u64(id, "invalid search document ID")?,
                            },
                            issues,
                            search_transformer_version: Some(version),
                            media_object: None,
                        });
                    }
                }
                4 => {
                    let mut statement = self.connection.prepare(
                        "SELECT search_fts.rowid,
                                EXISTS (
                                    SELECT 1 FROM search_documents AS document
                                    WHERE document.search_id = search_fts.rowid
                                )
                         FROM search_fts WHERE search_fts.rowid > ?1
                         ORDER BY search_fts.rowid LIMIT ?2",
                    )?;
                    let rows = statement.query_map(params![first_key, remaining], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
                    })?;
                    for row in rows {
                        let (row_id, document) = row?;
                        records.push(IntegrityMetadataRecord {
                            subject: IntegrityMetadataSubject::SearchFtsRow { row_id },
                            issues: if document {
                                Vec::new()
                            } else {
                                vec![IntegrityMetadataIssue::SearchFtsRowOrphan]
                            },
                            search_transformer_version: None,
                            media_object: None,
                        });
                    }
                }
                5 => {
                    let mut statement = self.connection.prepare(
                        "WITH RECURSIVE control_codes(code) AS (
                             SELECT 0
                             UNION ALL SELECT code + 1 FROM control_codes WHERE code < 31
                             UNION ALL SELECT 127 FROM control_codes WHERE code = 31
                             UNION ALL SELECT code + 1 FROM control_codes
                                       WHERE code BETWEEN 127 AND 158
                         )
                         SELECT media.rowid, media.wiki_id, media.source_media_id,
                                substr(media.source_sha1, 1, 129),
                                length(CAST(media.source_sha1 AS BLOB)) BETWEEN 1 AND 128
                                  AND media.source_sha1 NOT GLOB '*[^A-Za-z0-9]*',
                                media.wiki_id > 0 AND media.source_media_id > 0
                                  AND length(CAST(media.file_title AS BLOB)) BETWEEN 1 AND 16384
                                  AND length(CAST(media.original_url AS BLOB)) BETWEEN 1 AND 16384
                                  AND length(CAST(media.description_url AS BLOB)) BETWEEN 1 AND 16384
                                  AND length(CAST(media.author AS BLOB)) BETWEEN 1 AND 16384
                                  AND length(CAST(media.attribution AS BLOB)) BETWEEN 1 AND 16384
                                  AND length(CAST(media.license_name AS BLOB)) BETWEEN 1 AND 16384
                                  AND (media.license_url IS NULL OR
                                       length(CAST(media.license_url AS BLOB)) BETWEEN 1 AND 16384)
                                  AND NOT EXISTS (
                                      SELECT 1 FROM control_codes
                                      WHERE instr(media.file_title, char(code)) > 0
                                         OR instr(media.original_url, char(code)) > 0
                                         OR instr(media.description_url, char(code)) > 0
                                         OR instr(media.author, char(code)) > 0
                                         OR instr(media.attribution, char(code)) > 0
                                         OR instr(media.license_name, char(code)) > 0
                                         OR (media.license_url IS NOT NULL
                                             AND instr(media.license_url, char(code)) > 0)
                                  ),
                                media.width BETWEEN 1 AND 4096
                                  AND media.height BETWEEN 1 AND 4096,
                                media.width, media.height,
                                substr(media.mime_type, 1, 32),
                                media.captured_at >= 0,
                                substr(media.content_object_id, 1, 128),
                                length(CAST(media.content_object_id AS BLOB)) <= 128,
                                object.object_id IS NOT NULL,
                                object.object_kind IS 'media',
                                object.media_type IS 'application/octet-stream'
                                  AND object.uncompressed_length BETWEEN 1 AND 67108864
                         FROM media
                         LEFT JOIN content_objects AS object
                           ON object.object_id = media.content_object_id
                         WHERE media.rowid > ?1
                         ORDER BY media.rowid LIMIT ?2",
                    )?;
                    let rows = statement.query_map(params![first_key, remaining], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, bool>(4)?,
                            row.get::<_, bool>(5)?,
                            row.get::<_, bool>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, i64>(8)?,
                            row.get::<_, String>(9)?,
                            row.get::<_, bool>(10)?,
                            row.get::<_, String>(11)?,
                            row.get::<_, bool>(12)?,
                            row.get::<_, bool>(13)?,
                            row.get::<_, bool>(14)?,
                            row.get::<_, bool>(15)?,
                        ))
                    })?;
                    for row in rows {
                        let (
                            row_id,
                            wiki_id,
                            source_media_id,
                            source_hash_prefix,
                            source_hash_valid,
                            text_metadata_valid,
                            dimensions_valid,
                            width,
                            height,
                            mime_type,
                            captured_at_valid,
                            object_id_text,
                            object_id_bounded,
                            object_exists,
                            object_kind_valid,
                            object_metadata_valid,
                        ) = row?;
                        let object_id = object_id_bounded
                            .then(|| object_id_text.parse::<ObjectId>().ok())
                            .flatten();
                        let mut issues = Vec::new();
                        if !object_exists {
                            issues.push(IntegrityMetadataIssue::MediaObjectMissing);
                        } else if !object_kind_valid {
                            issues.push(IntegrityMetadataIssue::MediaObjectWrongKind);
                        }
                        if !source_hash_valid
                            || !text_metadata_valid
                            || !dimensions_valid
                            || !matches!(mime_type.as_str(), "image/jpeg" | "image/png")
                            || !captured_at_valid
                            || object_id.is_none()
                            || (object_exists && !object_metadata_valid)
                        {
                            issues.push(IntegrityMetadataIssue::MediaMetadataInvalid);
                        }
                        let media_object = object_id.map(|object_id| IntegrityMediaObject {
                            object_id,
                            mime_type,
                            width: u32::try_from(width).ok(),
                            height: u32::try_from(height).ok(),
                        });
                        records.push(IntegrityMetadataRecord {
                            subject: IntegrityMetadataSubject::Media {
                                row_id,
                                wiki_id,
                                source_media_id,
                                source_hash_prefix,
                            },
                            issues,
                            search_transformer_version: None,
                            media_object,
                        });
                    }
                }
                6 => {
                    let mut statement = self.connection.prepare(
                        "WITH RECURSIVE control_codes(code) AS (
                             SELECT 0
                             UNION ALL SELECT code + 1 FROM control_codes WHERE code < 31
                             UNION ALL SELECT 127 FROM control_codes WHERE code = 31
                             UNION ALL SELECT code + 1 FROM control_codes
                                       WHERE code BETWEEN 127 AND 158
                         )
                         SELECT placement.rowid, placement.wiki_id,
                                placement.revision_id, placement.placement_index,
                                placement.wiki_id > 0 AND placement.revision_id > 0
                                  AND placement.placement_index BETWEEN 0 AND 255
                                  AND placement.source_media_id > 0
                                  AND length(CAST(placement.source_sha1 AS BLOB)) BETWEEN 1 AND 128
                                  AND placement.source_sha1 NOT GLOB '*[^A-Za-z0-9]*'
                                  AND placement.placement_kind IN ('lead', 'inline')
                                  AND (placement.caption IS NULL OR
                                       length(CAST(placement.caption AS BLOB)) BETWEEN 1 AND 16384)
                                  AND (placement.alt_text IS NULL OR
                                       length(CAST(placement.alt_text AS BLOB)) BETWEEN 1 AND 16384)
                                  AND NOT EXISTS (
                                      SELECT 1 FROM control_codes
                                      WHERE (placement.caption IS NOT NULL
                                             AND instr(placement.caption, char(code)) > 0)
                                         OR (placement.alt_text IS NOT NULL
                                             AND instr(placement.alt_text, char(code)) > 0)
                                  ),
                                revision.revision_id IS NOT NULL,
                                page.page_id IS NOT NULL,
                                media.source_media_id IS NOT NULL
                         FROM page_media AS placement
                         LEFT JOIN revisions AS revision
                           ON revision.wiki_id = placement.wiki_id
                          AND revision.revision_id = placement.revision_id
                         LEFT JOIN pages AS page
                           ON page.wiki_id = revision.wiki_id
                          AND page.page_id = revision.page_id
                         LEFT JOIN media
                          ON media.wiki_id = placement.wiki_id
                          AND media.source_media_id = placement.source_media_id
                          AND media.source_sha1 = placement.source_sha1
                          AND media.content_object_id = placement.content_object_id
                         WHERE placement.rowid > ?1
                         ORDER BY placement.rowid LIMIT ?2",
                    )?;
                    let rows = statement.query_map(params![first_key, remaining], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, bool>(4)?,
                            row.get::<_, bool>(5)?,
                            row.get::<_, bool>(6)?,
                            row.get::<_, bool>(7)?,
                        ))
                    })?;
                    for row in rows {
                        let (
                            row_id,
                            wiki_id,
                            revision_id,
                            placement_index,
                            metadata_valid,
                            revision_exists,
                            page_exists,
                            media_exists,
                        ) = row?;
                        let mut issues = Vec::new();
                        if !metadata_valid {
                            issues.push(IntegrityMetadataIssue::PageMediaMetadataInvalid);
                        }
                        if !revision_exists {
                            issues.push(IntegrityMetadataIssue::PageMediaRevisionMissing);
                        } else if !page_exists {
                            issues.push(IntegrityMetadataIssue::PageMediaPageMissing);
                        }
                        if !media_exists {
                            issues.push(IntegrityMetadataIssue::PageMediaMediaMissing);
                        }
                        records.push(IntegrityMetadataRecord {
                            subject: IntegrityMetadataSubject::PageMedia {
                                row_id,
                                wiki_id,
                                revision_id,
                                placement_index,
                            },
                            issues,
                            search_transformer_version: None,
                            media_object: None,
                        });
                    }
                }
                _ => break,
            }

            if records.len() == before + remaining as usize {
                break;
            }
            category += 1;
            first_key = -1;
            second_key = -1;
        }
        Ok(records)
    }

    /// Reads, bounds, decompresses, and verifies a canonical object.
    pub fn read_object(&self, id: ObjectId) -> Result<Vec<u8>, StoreError> {
        let (kind, expected_length, locations) = self.object_locations(id)?;
        let mut first_error = None;
        for location in locations {
            let result = if location.storage_kind == "loose" {
                self.read_loose_location(id, kind, expected_length, &location.relative_path)
            } else {
                self.read_pack_location(id, kind, expected_length, &location)
            };
            match result {
                Ok(bytes) => return Ok(bytes),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        Err(first_error.unwrap_or(StoreError::ObjectNotFound(id)))
    }

    fn object_locations(
        &self,
        id: ObjectId,
    ) -> Result<(ObjectKind, u64, Vec<PackLocation>), StoreError> {
        let logical: Option<(String, i64)> = self
            .connection
            .query_row(
                "SELECT object_kind, uncompressed_length
                 FROM content_objects
                 WHERE object_id = ?1 AND verification_state = 'verified'",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (raw_kind, raw_length) = logical.ok_or(StoreError::ObjectNotFound(id))?;
        let kind = ObjectKind::from_database(&raw_kind)?;
        let expected_length = u64::try_from(raw_length)
            .map_err(|_| StoreError::CorruptMetadata("negative object length"))?;
        if expected_length > self.config.max_object_bytes {
            return Err(StoreError::ObjectTooLarge {
                limit: self.config.max_object_bytes,
                actual: expected_length,
            });
        }
        let mut statement = self.connection.prepare(
            "SELECT locations.storage_kind, locations.encoding,
                    locations.relative_path, locations.compressed_length,
                    locations.base_object_id, locations.pack_id,
                    locations.pack_offset, locations.delta_depth,
                    packs.pack_path, packs.index_path, packs.pack_checksum,
                    packs.index_checksum
             FROM object_locations AS locations
             LEFT JOIN packs ON packs.pack_id = locations.pack_id
             WHERE locations.object_id = ?1
               AND locations.verification_state = 'verified'
               AND (locations.storage_kind = 'loose' OR packs.state = 'verified')
             ORDER BY (locations.storage_kind = 'pack') DESC,
                      locations.pack_generation DESC, locations.location_id DESC",
        )?;
        let locations = statement
            .query_map([id.to_string()], |row| {
                Ok(PackLocation {
                    storage_kind: row.get(0)?,
                    encoding: row.get(1)?,
                    relative_path: row.get(2)?,
                    compressed_length: row.get(3)?,
                    base_object_id: row.get(4)?,
                    pack_id: row.get(5)?,
                    pack_offset: row.get(6)?,
                    delta_depth: row.get(7)?,
                    pack_path: row.get(8)?,
                    index_path: row.get(9)?,
                    pack_checksum: row.get(10)?,
                    index_checksum: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((kind, expected_length, locations))
    }

    fn read_loose_object(&self, id: ObjectId) -> Result<(ObjectKind, u64, Vec<u8>), StoreError> {
        let metadata: Option<(String, i64, String)> = self
            .connection
            .query_row(
                "SELECT objects.object_kind, objects.uncompressed_length,
                        locations.relative_path
                 FROM content_objects AS objects
                 JOIN object_locations AS locations USING (object_id)
                 WHERE objects.object_id = ?1
                   AND objects.verification_state = 'verified'
                   AND locations.storage_kind = 'loose'
                   AND locations.encoding = 'zstd'
                   AND locations.verification_state = 'verified'
                 ORDER BY locations.location_id DESC LIMIT 1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (raw_kind, raw_length, relative_path) =
            metadata.ok_or(StoreError::ObjectNotFound(id))?;
        let kind = ObjectKind::from_database(&raw_kind)?;
        let expected_length = u64::try_from(raw_length)
            .map_err(|_| StoreError::CorruptMetadata("negative object length"))?;
        let bytes = self.read_loose_location(id, kind, expected_length, &relative_path)?;
        Ok((kind, expected_length, bytes))
    }

    fn read_loose_location(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        expected_length: u64,
        raw_path: &str,
    ) -> Result<Vec<u8>, StoreError> {
        if expected_length > self.config.max_object_bytes {
            return Err(StoreError::ObjectTooLarge {
                limit: self.config.max_object_bytes,
                actual: expected_length,
            });
        }
        let relative_path = loose_database_path(raw_path)?;
        let file = File::open(self.root.join(relative_path))?;
        let decoder = zstd::stream::read::Decoder::new(file)?;
        let bytes = read_bounded(decoder, expected_length)?;
        verify_object_bytes(id, kind, expected_length, &bytes)?;
        Ok(bytes)
    }

    fn read_pack_location(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        expected_length: u64,
        location: &PackLocation,
    ) -> Result<Vec<u8>, StoreError> {
        let pack_id = location
            .pack_id
            .as_deref()
            .ok_or(StoreError::CorruptMetadata("pack location lacks pack ID"))?;
        let pack_path = pack_database_path(
            location
                .pack_path
                .as_deref()
                .ok_or(StoreError::CorruptMetadata("pack location lacks pack path"))?,
            ".pack",
        )?;
        if location.relative_path != pack_path.to_string_lossy() {
            return Err(StoreError::CorruptMetadata(
                "pack location path disagrees with pack",
            ));
        }
        let index_path = pack_database_path(
            location
                .index_path
                .as_deref()
                .ok_or(StoreError::CorruptMetadata(
                    "pack location lacks index path",
                ))?,
            ".idx",
        )?;
        let pack_checksum = parse_checksum(
            location
                .pack_checksum
                .as_deref()
                .ok_or(StoreError::CorruptMetadata("pack lacks checksum"))?,
        )?;
        if format!("b3:{}", blake3::Hash::from_bytes(pack_checksum).to_hex()) != pack_id {
            return Err(StoreError::CorruptMetadata(
                "pack identity disagrees with checksum",
            ));
        }
        let index_checksum = parse_checksum(
            location
                .index_checksum
                .as_deref()
                .ok_or(StoreError::CorruptMetadata("pack index lacks checksum"))?,
        )?;
        let index = read_pack_index(
            &self.root.join(index_path),
            pack_checksum,
            index_checksum,
            u64::from(MAX_SUPPORTED_PACK_OBJECTS),
        )?;
        let indexed = index
            .iter()
            .find(|entry| entry.id == id)
            .ok_or(StoreError::CorruptPack("object is absent from pack index"))?;
        if Some(to_sql_integer(indexed.offset)?) != location.pack_offset {
            return Err(StoreError::CorruptMetadata(
                "pack offset disagrees with index",
            ));
        }
        if to_sql_integer(indexed.record_length)? != location.compressed_length {
            return Err(StoreError::CorruptMetadata(
                "pack record length disagrees with index",
            ));
        }
        let pack_absolute = self.root.join(&pack_path);
        let decoded = read_pack_entry(&mut File::open(&pack_absolute)?, *indexed)?;
        if decoded.id != id
            || decoded.kind != kind
            || decoded.uncompressed_length != expected_length
            || decoded.encoding.database_value() != location.encoding
            || decoded.base_id.map(|base| base.to_string()) != location.base_object_id
            || Some(i64::from(decoded.delta_depth)) != location.delta_depth
        {
            return Err(StoreError::CorruptMetadata(
                "pack location disagrees with indexed entry",
            ));
        }
        let mut cache = HashMap::new();
        let mut depths = HashMap::new();
        let bytes = reconstruct_pack_object(
            &pack_absolute,
            &index,
            id,
            self.config.max_object_bytes,
            0,
            &mut cache,
            &mut depths,
        )?;
        let entry_depth = depths
            .get(&id)
            .copied()
            .ok_or(StoreError::CorruptPack("reconstructed object lacks depth"))?;
        if Some(i64::from(entry_depth)) != location.delta_depth {
            return Err(StoreError::CorruptMetadata(
                "delta depth disagrees with pack",
            ));
        }
        let expected_encoding = if entry_depth == 0 {
            "pack-full"
        } else {
            "pack-delta"
        };
        if location.encoding != expected_encoding {
            return Err(StoreError::CorruptMetadata(
                "pack encoding disagrees with entry",
            ));
        }
        verify_object_bytes(id, kind, expected_length, &bytes)?;
        Ok(bytes)
    }

    /// Returns the current SQLite schema version.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        self.connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    #[cfg(test)]
    fn connection(&self) -> &Connection {
        &self.connection
    }
}

type ScheduleRow = (
    i64,
    String,
    Option<i64>,
    i64,
    bool,
    Option<i64>,
    Option<i64>,
);
type SyncJobRow = (i64, i64, String, String, Option<String>, String, i64, bool);
type SyncCheckpointRow = (
    i64,
    Option<i64>,
    i64,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    i64,
);
type SyncRunStatusRow = (
    i64,
    i64,
    Option<i64>,
    String,
    String,
    i64,
    i64,
    Option<String>,
    i64,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<bool>,
    Option<i64>,
);

fn schedule_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn stored_schedule(row: ScheduleRow) -> Result<CollectionSchedule, StoreError> {
    let (
        collection_id,
        cadence_kind,
        cadence_seconds,
        jitter_seconds,
        paused,
        next_run_at,
        last_started_at,
    ) = row;
    let cadence = match cadence_kind.as_str() {
        "manual" if cadence_seconds.is_none() => ScheduleCadence::Manual,
        "interval" => {
            let seconds = cadence_seconds.ok_or(StoreError::CorruptMetadata(
                "interval schedule lacks cadence seconds",
            ))?;
            let seconds = u32::try_from(seconds)
                .map_err(|_| StoreError::CorruptMetadata("invalid schedule interval"))?;
            ScheduleCadence::Interval(
                ScheduleInterval::new(seconds)
                    .map_err(|_| StoreError::CorruptMetadata("invalid schedule interval"))?,
            )
        }
        "daily-utc" => {
            let seconds = cadence_seconds
                .ok_or(StoreError::CorruptMetadata("daily schedule lacks UTC time"))?;
            let seconds = u32::try_from(seconds)
                .map_err(|_| StoreError::CorruptMetadata("invalid daily UTC time"))?;
            ScheduleCadence::DailyUtc(
                DailyUtcTime::new(seconds)
                    .map_err(|_| StoreError::CorruptMetadata("invalid daily UTC time"))?,
            )
        }
        "manual" => {
            return Err(StoreError::CorruptMetadata(
                "manual schedule has cadence seconds",
            ));
        }
        _ => return Err(StoreError::CorruptMetadata("unknown schedule cadence")),
    };
    let jitter_seconds = u32::try_from(jitter_seconds)
        .map_err(|_| StoreError::CorruptMetadata("invalid schedule jitter"))?;
    let next_run_at = next_run_at
        .map(|value| sql_u64(value, "invalid next schedule time"))
        .transpose()?;
    validate_schedule_configuration(cadence, jitter_seconds, next_run_at)
        .map_err(|_| StoreError::CorruptMetadata("invalid schedule configuration"))?;
    Ok(CollectionSchedule {
        collection_id: sql_id(collection_id, "invalid collection ID in schedule")?,
        cadence,
        jitter_seconds,
        paused,
        next_run_at,
        last_started_at: last_started_at
            .map(|value| sql_u64(value, "invalid last schedule start time"))
            .transpose()?,
    })
}

fn sync_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncJobRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn stored_sync_job(row: SyncJobRow) -> Result<SyncJob, StoreError> {
    Ok(SyncJob {
        job_id: sql_u64(row.0, "invalid sync job ID")?,
        run_id: sql_u64(row.1, "invalid sync run ID")?,
        key: row.2,
        kind: row.3,
        subject: row.4,
        state: SyncJobState::from_database(&row.5)?,
        attempt_count: u32::try_from(row.6)
            .map_err(|_| StoreError::CorruptMetadata("invalid sync job attempt count"))?,
        retryable: row.7,
    })
}

fn sync_checkpoint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncCheckpointRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn stored_sync_checkpoint(row: SyncCheckpointRow) -> Result<SyncCheckpoint, StoreError> {
    Ok(SyncCheckpoint {
        wiki_id: sql_id(row.0, "invalid stored wiki ID")?,
        collection_id: row
            .1
            .map(|value| sql_id(value, "invalid collection ID"))
            .transpose()?,
        committed_through: sql_u64(row.2, "invalid sync checkpoint")?,
        overlap_seconds: sql_u64(row.3, "invalid sync overlap")?,
        recent_changes_cursor: row.4,
        reconciled_at: row
            .5
            .map(|value| sql_u64(value, "invalid reconciliation time"))
            .transpose()?,
        last_run_id: row
            .6
            .map(|value| sql_u64(value, "invalid last sync run ID"))
            .transpose()?,
        updated_at: sql_u64(row.7, "invalid sync checkpoint update time")?,
    })
}

fn sync_run_status_query() -> &'static str {
    "SELECT runs.run_id, runs.wiki_id, runs.collection_id, runs.run_kind,
            runs.state, runs.window_start, runs.checkpoint_candidate,
            runs.configuration_hash,
            (SELECT COUNT(*) FROM sync_jobs WHERE run_id = runs.run_id AND state = 'queued'),
            (SELECT COUNT(*) FROM sync_jobs WHERE run_id = runs.run_id AND state = 'running'),
            (SELECT COUNT(*) FROM sync_jobs WHERE run_id = runs.run_id AND state = 'succeeded'),
            (SELECT COUNT(*) FROM sync_jobs WHERE run_id = runs.run_id AND state = 'failed'),
            runs.created_at, runs.finished_at,
            latest_error.code, latest_error.message, latest_error.retryable,
            latest_error.occurred_at
     FROM sync_runs AS runs
     LEFT JOIN sync_errors AS latest_error ON latest_error.error_id = (
         SELECT errors.error_id FROM sync_errors AS errors
         WHERE errors.run_id = runs.run_id
         ORDER BY errors.occurred_at DESC, errors.error_id DESC LIMIT 1
     )"
}

fn sync_run_status_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncRunStatusRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
    ))
}

fn stored_sync_run_status(row: SyncRunStatusRow) -> Result<SyncRunStatus, StoreError> {
    Ok(SyncRunStatus {
        run_id: sql_u64(row.0, "invalid sync run ID")?,
        wiki_id: sql_id(row.1, "invalid stored wiki ID")?,
        collection_id: row
            .2
            .map(|value| sql_id(value, "invalid collection ID"))
            .transpose()?,
        kind: SyncRunKind::from_database(&row.3)?,
        state: SyncRunState::from_database(&row.4)?,
        window_start: sql_u64(row.5, "invalid sync window start")?,
        checkpoint_candidate: sql_u64(row.6, "invalid checkpoint candidate")?,
        configuration_hash: row.7,
        queued_jobs: sql_u64(row.8, "invalid queued job count")?,
        running_jobs: sql_u64(row.9, "invalid running job count")?,
        succeeded_jobs: sql_u64(row.10, "invalid succeeded job count")?,
        failed_jobs: sql_u64(row.11, "invalid failed job count")?,
        created_at: sql_u64(row.12, "invalid sync creation time")?,
        finished_at: row
            .13
            .map(|value| sql_u64(value, "invalid sync finish time"))
            .transpose()?,
        latest_error: match (row.14, row.15, row.16, row.17) {
            (None, None, None, None) => None,
            (Some(code), Some(message), Some(retryable), Some(occurred_at)) => Some(SyncFailure {
                code,
                message,
                retryable,
                occurred_at: sql_u64(occurred_at, "invalid sync error time")?,
            }),
            _ => return Err(StoreError::CorruptMetadata("incomplete sync error")),
        },
    })
}

fn dump_import_status_query() -> &'static str {
    "SELECT import_id, run_id, wiki_id, collection_id, dump_digest,
            dump_compressed_bytes, collection_generation, configuration_hash,
            bootstrap_started_at, state, pages_scanned, imported_pages,
            imported_canonical_bytes, attempt_count, retryable, error_code,
            error_message, created_at, claimed_at, updated_at, finished_at
     FROM dump_imports"
}

fn dump_import_status_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DumpImportRow> {
    Ok(DumpImportRow {
        import_id: row.get(0)?,
        run_id: row.get(1)?,
        wiki_id: row.get(2)?,
        collection_id: row.get(3)?,
        dump_digest: row.get(4)?,
        dump_compressed_bytes: row.get(5)?,
        collection_generation: row.get(6)?,
        configuration_hash: row.get(7)?,
        bootstrap_started_at: row.get(8)?,
        state: row.get(9)?,
        pages_scanned: row.get(10)?,
        imported_pages: row.get(11)?,
        imported_canonical_bytes: row.get(12)?,
        attempt_count: row.get(13)?,
        retryable: row.get(14)?,
        error_code: row.get(15)?,
        error_message: row.get(16)?,
        created_at: row.get(17)?,
        claimed_at: row.get(18)?,
        updated_at: row.get(19)?,
        finished_at: row.get(20)?,
    })
}

fn stored_dump_import_status(row: DumpImportRow) -> Result<DumpImportStatus, StoreError> {
    let state = DumpImportState::from_database(&row.state)?;
    let finished_at = row
        .finished_at
        .map(|value| sql_u64(value, "invalid dump import finish time"))
        .transpose()?;
    let latest_error = match (row.error_code, row.error_message, finished_at) {
        (None, None, _) => None,
        (Some(code), Some(message), Some(occurred_at)) if state == DumpImportState::Failed => {
            Some(SyncFailure {
                code,
                message,
                retryable: row.retryable,
                occurred_at,
            })
        }
        _ => {
            return Err(StoreError::CorruptMetadata("incomplete dump import error"));
        }
    };
    Ok(DumpImportStatus {
        import_id: sql_u64(row.import_id, "invalid dump import ID")?,
        run_id: sql_u64(row.run_id, "invalid dump import run ID")?,
        wiki_id: sql_id(row.wiki_id, "invalid dump import wiki ID")?,
        collection_id: sql_id(row.collection_id, "invalid dump import collection ID")?,
        dump_digest: row.dump_digest,
        dump_compressed_bytes: sql_u64(
            row.dump_compressed_bytes,
            "invalid compressed dump length",
        )?,
        collection_generation: sql_u64(
            row.collection_generation,
            "invalid dump import collection generation",
        )?,
        configuration_hash: row.configuration_hash,
        bootstrap_started_at: sql_u64(
            row.bootstrap_started_at,
            "invalid dump bootstrap start time",
        )?,
        state,
        pages_scanned: sql_u64(row.pages_scanned, "invalid dump page cursor")?,
        imported_pages: sql_u64(row.imported_pages, "invalid imported page count")?,
        imported_canonical_bytes: sql_u64(
            row.imported_canonical_bytes,
            "invalid imported canonical byte count",
        )?,
        attempt_count: u32::try_from(row.attempt_count)
            .map_err(|_| StoreError::CorruptMetadata("invalid dump import attempt count"))?,
        retryable: row.retryable,
        created_at: sql_u64(row.created_at, "invalid dump import creation time")?,
        claimed_at: sql_u64(row.claimed_at, "invalid dump import claim time")?,
        updated_at: sql_u64(row.updated_at, "invalid dump import update time")?,
        finished_at,
        latest_error,
    })
}

fn dump_import_status_by_id(
    connection: &Connection,
    raw_import_id: i64,
) -> Result<DumpImportStatus, StoreError> {
    let raw = connection
        .query_row(
            &format!("{} WHERE import_id = ?1", dump_import_status_query()),
            [raw_import_id],
            dump_import_status_row,
        )
        .optional()?;
    raw.map(stored_dump_import_status)
        .transpose()?
        .ok_or_else(|| {
            StoreError::DumpImportNotRunning(u64::try_from(raw_import_id).unwrap_or_default())
        })
}

fn ensure_dump_progress_can_advance(
    connection: &Connection,
    import_id: u64,
    raw_import_id: i64,
    pages_scanned: u64,
) -> Result<(i64, i64, i64, i64), StoreError> {
    let current: Option<(i64, i64, i64, i64, i64)> = connection
        .query_row(
            "SELECT wiki_id, collection_id, pages_scanned,
                    imported_pages, imported_canonical_bytes
             FROM dump_imports WHERE import_id = ?1 AND state = 'running'",
            [raw_import_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((wiki_id, collection_id, current_cursor, imported_pages, imported_bytes)) = current
    else {
        return Err(StoreError::DumpImportNotRunning(import_id));
    };
    let current_cursor = sql_u64(current_cursor, "invalid dump page cursor")?;
    if pages_scanned < current_cursor {
        return Err(StoreError::DumpImportProgressRegression {
            import_id,
            current: current_cursor,
            requested: pages_scanned,
        });
    }
    Ok((wiki_id, collection_id, imported_pages, imported_bytes))
}

fn validate_dump_digest(value: &str) -> Result<(), StoreError> {
    if value.len() != 67
        || !value.starts_with("b3:")
        || !value.as_bytes()[3..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(StoreError::InvalidDumpIdentity(
            "dump digest must be b3: followed by 64 lowercase hexadecimal digits",
        ));
    }
    Ok(())
}

fn validate_sync_text(value: &str, label: &'static str) -> Result<(), StoreError> {
    const MAX_SYNC_TEXT_BYTES: usize = 8 * 1024;
    if value.trim().is_empty()
        || value.len() > MAX_SYNC_TEXT_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(StoreError::InvalidSyncText(label));
    }
    Ok(())
}

fn validate_thumbnail_capture(
    policy: ThumbnailPolicy,
    capture: &ThumbnailCapture<'_>,
    placement: RevisionMediaPlacement<'_>,
) -> Result<(), StoreError> {
    for (value, label) in [
        (capture.file_title.as_str(), "media file title"),
        (capture.source_sha1, "media source hash"),
        (capture.original_url, "media original URL"),
        (capture.description_url, "media description URL"),
        (capture.author, "media author"),
        (capture.attribution, "media attribution"),
        (capture.license_name, "media license"),
    ] {
        validate_media_text(value, label)?;
    }
    if capture.source_sha1.len() > MAX_MEDIA_SOURCE_HASH_BYTES
        || !capture
            .source_sha1
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(StoreError::InvalidMediaMetadata(
            "media source hash must be bounded and alphanumeric",
        ));
    }
    if let Some(value) = capture.license_url {
        validate_media_text(value, "media license URL")?;
    }
    if let Some(value) = placement.caption {
        validate_media_text(value, "media caption")?;
    }
    if let Some(value) = placement.alt_text {
        validate_media_text(value, "media alternative text")?;
    }
    let maximum_edge = policy.maximum_edge_pixels().get();
    if capture.width == 0
        || capture.height == 0
        || capture.width > maximum_edge
        || capture.height > maximum_edge
    {
        return Err(StoreError::InvalidMediaMetadata(
            "media dimensions exceed the thumbnail policy",
        ));
    }
    if placement.index >= policy.maximum_images_per_revision().get() {
        return Err(StoreError::InvalidMediaMetadata(
            "media placement exceeds the thumbnail-count policy",
        ));
    }
    let validated = validate_thumbnail(
        capture.source,
        capture.mime_type.as_str(),
        &thumbnail_limits(policy),
    )
    .map_err(|_| {
        StoreError::InvalidMediaMetadata(
            "thumbnail failed complete bounded passive-raster validation",
        )
    })?;
    if validated.width != capture.width || validated.height != capture.height {
        return Err(StoreError::InvalidMediaMetadata(
            "decoded thumbnail dimensions disagree with media metadata",
        ));
    }
    to_sql_integer(capture.captured_at)?;
    Ok(())
}

fn thumbnail_limits(policy: ThumbnailPolicy) -> ThumbnailLimits {
    let maximum_edge = policy.maximum_edge_pixels().get();
    let maximum_pixels = u64::from(maximum_edge) * u64::from(maximum_edge);
    ThumbnailLimits {
        max_encoded_bytes: policy.maximum_bytes_per_image().get(),
        max_width: maximum_edge,
        max_height: maximum_edge,
        max_pixels: maximum_pixels,
        max_decoded_bytes: maximum_pixels * MAX_THUMBNAIL_BYTES_PER_PIXEL,
    }
}

fn validate_media_text(value: &str, _label: &'static str) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > MAX_MEDIA_METADATA_TEXT_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(StoreError::InvalidMediaMetadata(
            "media text must be non-empty, bounded, and contain no controls",
        ));
    }
    Ok(())
}

fn expected_media_metadata(capture: &ThumbnailCapture<'_>) -> MediaMetadataRow {
    MediaMetadataRow {
        file_title: capture.file_title.as_str().to_owned(),
        original_url: capture.original_url.to_owned(),
        description_url: capture.description_url.to_owned(),
        author: capture.author.to_owned(),
        attribution: capture.attribution.to_owned(),
        license_name: capture.license_name.to_owned(),
        license_url: capture.license_url.map(str::to_owned),
        width: i64::from(capture.width),
        height: i64::from(capture.height),
        mime_type: capture.mime_type.as_str().to_owned(),
    }
}

fn query_media_metadata(
    connection: &Connection,
    wiki_id: i64,
    media_id: i64,
    source_sha1: &str,
    object_id: ObjectId,
) -> Result<Option<MediaMetadataRow>, StoreError> {
    connection
        .query_row(
            "SELECT file_title, original_url, description_url, author,
                    attribution, license_name, license_url, width, height,
                    mime_type
             FROM media
             WHERE wiki_id = ?1 AND source_media_id = ?2 AND source_sha1 = ?3
               AND content_object_id = ?4",
            params![wiki_id, media_id, source_sha1, object_id.to_string()],
            |row| {
                Ok(MediaMetadataRow {
                    file_title: row.get(0)?,
                    original_url: row.get(1)?,
                    description_url: row.get(2)?,
                    author: row.get(3)?,
                    attribution: row.get(4)?,
                    license_name: row.get(5)?,
                    license_url: row.get(6)?,
                    width: row.get(7)?,
                    height: row.get(8)?,
                    mime_type: row.get(9)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

type MediaPlacementRow = (i64, String, String, String, Option<String>, Option<String>);

fn query_media_placement(
    connection: &Connection,
    wiki_id: i64,
    revision_id: i64,
    placement_index: i64,
) -> Result<Option<MediaPlacementRow>, StoreError> {
    connection
        .query_row(
            "SELECT source_media_id, source_sha1, content_object_id,
                    placement_kind, caption, alt_text
             FROM page_media
             WHERE wiki_id = ?1 AND revision_id = ?2 AND placement_index = ?3",
            params![wiki_id, revision_id, placement_index],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn revision_media_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RevisionMediaRow> {
    Ok(RevisionMediaRow {
        placement_index: row.get(0)?,
        placement_kind: row.get(1)?,
        caption: row.get(2)?,
        alt_text: row.get(3)?,
        source_media_id: row.get(4)?,
        source_sha1: row.get(5)?,
        file_title: row.get(6)?,
        original_url: row.get(7)?,
        description_url: row.get(8)?,
        author: row.get(9)?,
        attribution: row.get(10)?,
        license_name: row.get(11)?,
        license_url: row.get(12)?,
        width: row.get(13)?,
        height: row.get(14)?,
        mime_type: row.get(15)?,
        captured_at: row.get(16)?,
        content_object_id: row.get(17)?,
    })
}

fn stored_revision_media(
    wiki_id: WikiId,
    revision_id: RevisionId,
    row: RevisionMediaRow,
) -> Result<StoredRevisionMedia, StoreError> {
    Ok(StoredRevisionMedia {
        wiki_id,
        revision_id,
        placement_index: u32::try_from(row.placement_index)
            .map_err(|_| StoreError::CorruptMetadata("invalid media placement index"))?,
        placement_kind: MediaPlacementKind::from_database(&row.placement_kind)?,
        caption: row.caption,
        alt_text: row.alt_text,
        media_id: sql_id(row.source_media_id, "invalid stored media ID")?,
        file_title: PageTitle::new(row.file_title)
            .map_err(|_| StoreError::CorruptMetadata("invalid stored media title"))?,
        source_sha1: row.source_sha1,
        original_url: row.original_url,
        description_url: row.description_url,
        author: row.author,
        attribution: row.attribution,
        license_name: row.license_name,
        license_url: row.license_url,
        width: u32::try_from(row.width)
            .map_err(|_| StoreError::CorruptMetadata("invalid stored media width"))?,
        height: u32::try_from(row.height)
            .map_err(|_| StoreError::CorruptMetadata("invalid stored media height"))?,
        mime_type: ThumbnailMimeType::from_database(&row.mime_type)?,
        captured_at: sql_u64(row.captured_at, "invalid media capture time")?,
        content_object_id: row
            .content_object_id
            .parse()
            .map_err(|_| StoreError::CorruptMetadata("invalid media content object ID"))?,
    })
}

fn validate_mediawiki_timestamp(value: &str) -> Result<(), StoreError> {
    const ERROR: StoreError = StoreError::InvalidConfig(
        "MediaWiki timestamp must be a valid UTC value in YYYY-MM-DDTHH:MM:SSZ form",
    );
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return Err(ERROR);
    }
    let year = ascii_decimal(&bytes[0..4]).ok_or(ERROR)?;
    let month = ascii_decimal(&bytes[5..7]).ok_or(ERROR)?;
    let day = ascii_decimal(&bytes[8..10]).ok_or(ERROR)?;
    let hour = ascii_decimal(&bytes[11..13]).ok_or(ERROR)?;
    let minute = ascii_decimal(&bytes[14..16]).ok_or(ERROR)?;
    let second = ascii_decimal(&bytes[17..19]).ok_or(ERROR)?;
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return Err(ERROR),
    };
    if year == 0 || !(1..=maximum_day).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return Err(ERROR);
    }
    Ok(())
}

fn ascii_decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(byte - b'0'))
    })
}

fn insert_revision(
    transaction: &Transaction<'_>,
    wiki_id: i64,
    page_id: i64,
    capture: &RevisionCapture<'_>,
    object_id: ObjectId,
    captured_at: i64,
) -> Result<(), StoreError> {
    let revision_id = to_sql_integer(capture.revision_id.get())?;
    let parent_id = capture
        .parent_id
        .map(|id| to_sql_integer(id.get()))
        .transpose()?;
    let author_id = capture.author_id.map(to_sql_integer).transpose()?;
    let source_size = to_sql_integer(capture.source.len() as u64)?;
    transaction.execute(
        "INSERT OR IGNORE INTO revisions (
            wiki_id, revision_id, page_id, parent_revision_id, revision_time,
            author_name, author_id, comment, is_minor, source_size,
            upstream_sha1, content_model, content_object_id, captured_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
         )",
        params![
            wiki_id,
            revision_id,
            page_id,
            parent_id,
            capture.timestamp,
            capture.author,
            author_id,
            capture.comment,
            capture.minor,
            source_size,
            capture.upstream_sha1,
            capture.content_model,
            object_id.to_string(),
            captured_at,
        ],
    )?;
    let existing: (i64, Option<i64>, String, String) = transaction.query_row(
        "SELECT page_id, parent_revision_id, revision_time, content_object_id
         FROM revisions WHERE wiki_id = ?1 AND revision_id = ?2",
        params![wiki_id, revision_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if existing
        != (
            page_id,
            parent_id,
            capture.timestamp.to_owned(),
            object_id.to_string(),
        )
    {
        return Err(StoreError::ConflictingRevision(capture.revision_id));
    }
    Ok(())
}

#[derive(Debug)]
struct RevisionRow {
    wiki_id: i64,
    revision_id: i64,
    page_id: i64,
    parent_id: Option<i64>,
    timestamp: String,
    author: Option<String>,
    author_id: Option<i64>,
    comment: Option<String>,
    minor: bool,
    source_size: i64,
    upstream_sha1: Option<String>,
    content_model: String,
    object_id: String,
    captured_at: i64,
}

fn revision_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RevisionRow> {
    Ok(RevisionRow {
        wiki_id: row.get(0)?,
        revision_id: row.get(1)?,
        page_id: row.get(2)?,
        parent_id: row.get(3)?,
        timestamp: row.get(4)?,
        author: row.get(5)?,
        author_id: row.get(6)?,
        comment: row.get(7)?,
        minor: row.get(8)?,
        source_size: row.get(9)?,
        upstream_sha1: row.get(10)?,
        content_model: row.get(11)?,
        object_id: row.get(12)?,
        captured_at: row.get(13)?,
    })
}

fn stored_revision(row: RevisionRow) -> Result<(WikiId, StoredRevision), StoreError> {
    let wiki_id = sql_id(row.wiki_id, "invalid stored wiki ID")?;
    let revision_id = sql_id(row.revision_id, "invalid stored revision ID")?;
    let page_id = sql_id(row.page_id, "invalid stored page ID")?;
    let parent_id = row
        .parent_id
        .map(|value| sql_id(value, "invalid stored parent revision ID"))
        .transpose()?;
    let author_id = row
        .author_id
        .map(|value| {
            u64::try_from(value).map_err(|_| StoreError::CorruptMetadata("invalid author ID"))
        })
        .transpose()?;
    let source_size = u64::try_from(row.source_size)
        .map_err(|_| StoreError::CorruptMetadata("negative revision source size"))?;
    let captured_at = u64::try_from(row.captured_at)
        .map_err(|_| StoreError::CorruptMetadata("negative revision capture time"))?;
    Ok((
        wiki_id,
        StoredRevision {
            revision_id,
            page_id,
            parent_id,
            timestamp: row.timestamp,
            author: row.author,
            author_id,
            comment: row.comment,
            minor: row.minor,
            source_size,
            upstream_sha1: row.upstream_sha1,
            content_model: row.content_model,
            content_object_id: row
                .object_id
                .parse()
                .map_err(|_| StoreError::CorruptMetadata("invalid stored object ID"))?,
            captured_at,
        },
    ))
}

fn object_hasher(kind: ObjectKind, length: u64) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(OBJECT_DOMAIN);
    hasher.update(&[kind.identity_tag()]);
    hasher.update(&length.to_be_bytes());
    hasher
}

fn prepare_pack_entries(
    sources: &[PackSource],
    compression_level: i32,
) -> Result<Vec<PreparedPackEntry>, StoreError> {
    let mut entries: Vec<PreparedPackEntry> = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let full_payload = zstd::stream::encode_all(source.bytes.as_slice(), compression_level)?;
        let mut selected = (PackEncoding::Full, None, 0_u16, full_payload);
        let candidate_start = index.saturating_sub(DELTA_CANDIDATE_WINDOW);
        let starts_affinity_group = index == 0
            || sources[index - 1].kind != source.kind
            || sources[index - 1].affinity != source.affinity;
        if !starts_affinity_group && index % FULL_ENTRY_INTERVAL != 0 {
            for candidate_index in candidate_start..index {
                let base = &sources[candidate_index];
                let base_entry = &entries[candidate_index];
                if base.kind != source.kind
                    || base.affinity != source.affinity
                    || base_entry.delta_depth >= MAX_DELTA_DEPTH
                    || !delta_sizes_are_similar(base.bytes.len() as u64, source.bytes.len() as u64)
                {
                    continue;
                }
                let delta = create_delta(&base.bytes, &source.bytes);
                let payload = zstd::stream::encode_all(delta.as_slice(), compression_level)?;
                if payload
                    .len()
                    .checked_add(MIN_DELTA_SAVINGS)
                    .is_some_and(|length| length < selected.3.len())
                {
                    selected = (
                        PackEncoding::Delta,
                        Some(base.id),
                        base_entry.delta_depth + 1,
                        payload,
                    );
                }
            }
        }
        entries.push(PreparedPackEntry {
            id: source.id,
            kind: source.kind,
            uncompressed_length: source.bytes.len() as u64,
            encoding: selected.0,
            base_id: selected.1,
            delta_depth: selected.2,
            payload: selected.3,
            offset: 0,
            record_length: 0,
        });
    }
    Ok(entries)
}

fn object_size_class(length: u64) -> u32 {
    u64::BITS - length.max(1).leading_zeros() - 1
}

fn delta_sizes_are_similar(left: u64, right: u64) -> bool {
    let (smaller, larger) = if left <= right {
        (left, right)
    } else {
        (right, left)
    };
    larger <= smaller.saturating_mul(MAX_DELTA_SIZE_RATIO)
}

fn pack_affinity(
    wiki_id: Option<i64>,
    page_id: Option<i64>,
    revision_id: Option<i64>,
) -> Result<(Option<PackAffinity>, Option<u64>), StoreError> {
    match (wiki_id, page_id, revision_id) {
        (Some(wiki_id), Some(page_id), Some(revision_id)) => Ok((
            Some(PackAffinity {
                wiki_id: sql_u64(wiki_id, "invalid pack-affinity wiki ID")?,
                page_id: sql_u64(page_id, "invalid pack-affinity page ID")?,
            }),
            Some(sql_u64(revision_id, "invalid pack-affinity revision ID")?),
        )),
        (None, None, None) => Ok((None, None)),
        _ => Err(StoreError::CorruptMetadata(
            "pack affinity metadata is incomplete",
        )),
    }
}

fn create_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
    let prefix = base
        .iter()
        .zip(target)
        .take_while(|(left, right)| left == right)
        .count();
    let remaining_base = base.len().saturating_sub(prefix);
    let remaining_target = target.len().saturating_sub(prefix);
    let suffix = base
        .iter()
        .rev()
        .zip(target.iter().rev())
        .take(remaining_base.min(remaining_target))
        .take_while(|(left, right)| left == right)
        .count();
    let middle_end = target.len() - suffix;
    let mut delta = Vec::with_capacity(DELTA_HEADER_LENGTH + middle_end - prefix);
    delta.extend_from_slice(&(prefix as u64).to_be_bytes());
    delta.extend_from_slice(&(suffix as u64).to_be_bytes());
    delta.extend_from_slice(&target[prefix..middle_end]);
    delta
}

fn apply_delta(base: &[u8], delta: &[u8], expected_length: u64) -> Result<Vec<u8>, StoreError> {
    if delta.len() < DELTA_HEADER_LENGTH {
        return Err(StoreError::CorruptPack("delta header is truncated"));
    }
    let prefix = usize::try_from(u64::from_be_bytes(
        delta[0..8]
            .try_into()
            .map_err(|_| StoreError::CorruptPack("invalid delta prefix"))?,
    ))
    .map_err(|_| StoreError::CorruptPack("delta prefix exceeds host limits"))?;
    let suffix = usize::try_from(u64::from_be_bytes(
        delta[8..16]
            .try_into()
            .map_err(|_| StoreError::CorruptPack("invalid delta suffix"))?,
    ))
    .map_err(|_| StoreError::CorruptPack("delta suffix exceeds host limits"))?;
    if prefix > base.len() || suffix > base.len().saturating_sub(prefix) {
        return Err(StoreError::CorruptPack("delta exceeds its base object"));
    }
    let middle = &delta[DELTA_HEADER_LENGTH..];
    let actual_length = prefix
        .checked_add(middle.len())
        .and_then(|length| length.checked_add(suffix))
        .ok_or(StoreError::CorruptPack("delta output length overflow"))?;
    if actual_length as u64 != expected_length {
        return Err(StoreError::LengthMismatch {
            expected: expected_length,
            actual: actual_length as u64,
        });
    }
    let mut output = Vec::with_capacity(actual_length);
    output.extend_from_slice(&base[..prefix]);
    output.extend_from_slice(middle);
    output.extend_from_slice(&base[base.len() - suffix..]);
    Ok(output)
}

fn write_pack(
    file: &mut File,
    generation: u64,
    entries: &mut [PreparedPackEntry],
) -> Result<(), StoreError> {
    file.write_all(PACK_MAGIC)?;
    file.write_all(&generation.to_be_bytes())?;
    let mut offset = PACK_HEADER_LENGTH;
    for entry in entries {
        let payload_length = entry.payload.len() as u64;
        let record_length = PACK_ENTRY_HEADER_LENGTH
            .checked_add(payload_length)
            .ok_or(StoreError::PackLimitExceeded)?;
        entry.offset = offset;
        entry.record_length = record_length;
        file.write_all(&[entry.encoding.tag(), entry.kind.identity_tag()])?;
        file.write_all(&entry.delta_depth.to_be_bytes())?;
        file.write_all(entry.id.as_bytes())?;
        let base_bytes = entry.base_id.map_or([0_u8; 32], |id| *id.as_bytes());
        file.write_all(&base_bytes)?;
        file.write_all(&entry.uncompressed_length.to_be_bytes())?;
        file.write_all(&payload_length.to_be_bytes())?;
        file.write_all(&entry.payload)?;
        offset = offset
            .checked_add(record_length)
            .ok_or(StoreError::PackLimitExceeded)?;
    }
    Ok(())
}

fn write_pack_index(
    file: &mut File,
    pack_checksum: [u8; 32],
    entries: &[PreparedPackEntry],
) -> Result<(), StoreError> {
    file.write_all(INDEX_MAGIC)?;
    file.write_all(&pack_checksum)?;
    file.write_all(&(entries.len() as u64).to_be_bytes())?;
    let mut index: Vec<_> = entries
        .iter()
        .map(|entry| PackIndexEntry {
            id: entry.id,
            offset: entry.offset,
            record_length: entry.record_length,
        })
        .collect();
    index.sort_unstable_by_key(|entry| entry.id);
    for entry in index {
        file.write_all(entry.id.as_bytes())?;
        file.write_all(&entry.offset.to_be_bytes())?;
        file.write_all(&entry.record_length.to_be_bytes())?;
    }
    Ok(())
}

fn checksum_file(file: &mut File) -> Result<[u8; 32], StoreError> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn checksum_path(path: &Path) -> Result<[u8; 32], StoreError> {
    checksum_file(&mut File::open(path)?)
}

fn read_pack_index(
    path: &Path,
    expected_pack_checksum: [u8; 32],
    expected_index_checksum: [u8; 32],
    max_objects: u64,
) -> Result<Vec<PackIndexEntry>, StoreError> {
    if checksum_path(path)? != expected_index_checksum {
        return Err(StoreError::CorruptPack("pack index checksum mismatch"));
    }
    let mut file = File::open(path)?;
    let mut header = [0_u8; INDEX_HEADER_LENGTH as usize];
    file.read_exact(&mut header)?;
    if &header[0..8] != INDEX_MAGIC {
        return Err(StoreError::CorruptPack("invalid pack index magic"));
    }
    if header[8..40] != expected_pack_checksum {
        return Err(StoreError::CorruptPack("index refers to a different pack"));
    }
    let count = u64::from_be_bytes(
        header[40..48]
            .try_into()
            .map_err(|_| StoreError::CorruptPack("invalid pack index count"))?,
    );
    if count == 0 || count > max_objects {
        return Err(StoreError::CorruptPack("pack index count exceeds bounds"));
    }
    let expected_length = INDEX_HEADER_LENGTH
        .checked_add(
            INDEX_ENTRY_LENGTH
                .checked_mul(count)
                .ok_or(StoreError::CorruptPack("pack index length overflow"))?,
        )
        .ok_or(StoreError::CorruptPack("pack index length overflow"))?;
    if file.metadata()?.len() != expected_length {
        return Err(StoreError::CorruptPack("pack index length mismatch"));
    }
    let mut entries = Vec::with_capacity(
        usize::try_from(count)
            .map_err(|_| StoreError::CorruptPack("pack index exceeds host limits"))?,
    );
    for _ in 0..count {
        let mut raw = [0_u8; INDEX_ENTRY_LENGTH as usize];
        file.read_exact(&mut raw)?;
        let mut id = [0_u8; 32];
        id.copy_from_slice(&raw[0..32]);
        let offset = u64::from_be_bytes(
            raw[32..40]
                .try_into()
                .map_err(|_| StoreError::CorruptPack("invalid pack index offset"))?,
        );
        let record_length = u64::from_be_bytes(
            raw[40..48]
                .try_into()
                .map_err(|_| StoreError::CorruptPack("invalid pack record length"))?,
        );
        entries.push(PackIndexEntry {
            id: ObjectId(id),
            offset,
            record_length,
        });
    }
    if entries.windows(2).any(|pair| pair[0].id >= pair[1].id) {
        return Err(StoreError::CorruptPack("pack index is not strictly sorted"));
    }
    Ok(entries)
}

fn read_pack_entry(
    file: &mut File,
    indexed: PackIndexEntry,
) -> Result<DecodedPackEntry, StoreError> {
    if indexed.offset < PACK_HEADER_LENGTH || indexed.record_length < PACK_ENTRY_HEADER_LENGTH {
        return Err(StoreError::CorruptPack("invalid pack entry bounds"));
    }
    file.seek(SeekFrom::Start(indexed.offset))?;
    let mut header = [0_u8; PACK_ENTRY_HEADER_LENGTH as usize];
    file.read_exact(&mut header)?;
    let encoding = PackEncoding::from_tag(header[0])?;
    let kind = ObjectKind::from_identity_tag(header[1])?;
    let delta_depth = u16::from_be_bytes(
        header[2..4]
            .try_into()
            .map_err(|_| StoreError::CorruptPack("invalid delta depth"))?,
    );
    let mut id = [0_u8; 32];
    id.copy_from_slice(&header[4..36]);
    let mut raw_base = [0_u8; 32];
    raw_base.copy_from_slice(&header[36..68]);
    let uncompressed_length = u64::from_be_bytes(
        header[68..76]
            .try_into()
            .map_err(|_| StoreError::CorruptPack("invalid object length"))?,
    );
    let payload_length = u64::from_be_bytes(
        header[76..84]
            .try_into()
            .map_err(|_| StoreError::CorruptPack("invalid pack payload length"))?,
    );
    if PACK_ENTRY_HEADER_LENGTH.checked_add(payload_length) != Some(indexed.record_length) {
        return Err(StoreError::CorruptPack(
            "pack record length disagrees with index",
        ));
    }
    let base_id = (raw_base != [0_u8; 32]).then_some(ObjectId(raw_base));
    match encoding {
        PackEncoding::Full if delta_depth != 0 || base_id.is_some() => {
            return Err(StoreError::CorruptPack("invalid complete entry metadata"));
        }
        PackEncoding::Delta
            if delta_depth == 0 || delta_depth > MAX_DELTA_DEPTH || base_id.is_none() =>
        {
            return Err(StoreError::CorruptPack("invalid delta entry metadata"));
        }
        _ => {}
    }
    let mut payload = vec![
        0_u8;
        usize::try_from(payload_length).map_err(|_| StoreError::CorruptPack(
            "pack payload exceeds host limits"
        ))?
    ];
    file.read_exact(&mut payload)?;
    Ok(DecodedPackEntry {
        id: ObjectId(id),
        kind,
        uncompressed_length,
        encoding,
        base_id,
        delta_depth,
        payload,
    })
}

fn reconstruct_pack_object(
    pack_path: &Path,
    index: &[PackIndexEntry],
    id: ObjectId,
    max_object_bytes: u64,
    recursion_depth: u16,
    cache: &mut HashMap<ObjectId, Vec<u8>>,
    depths: &mut HashMap<ObjectId, u16>,
) -> Result<Vec<u8>, StoreError> {
    if let Some(bytes) = cache.get(&id) {
        return Ok(bytes.clone());
    }
    if recursion_depth > MAX_DELTA_DEPTH {
        return Err(StoreError::CorruptPack(
            "delta reconstruction depth exceeded",
        ));
    }
    let position = index
        .binary_search_by_key(&id, |entry| entry.id)
        .map_err(|_| StoreError::CorruptPack("delta base is absent from pack index"))?;
    let indexed = index[position];
    let mut file = File::open(pack_path)?;
    let entry = read_pack_entry(&mut file, indexed)?;
    if entry.id != id {
        return Err(StoreError::CorruptPack(
            "pack entry disagrees with index identity",
        ));
    }
    if entry.uncompressed_length > max_object_bytes {
        return Err(StoreError::ObjectTooLarge {
            limit: max_object_bytes,
            actual: entry.uncompressed_length,
        });
    }
    let bytes = match entry.encoding {
        PackEncoding::Full => {
            let decoder = zstd::stream::read::Decoder::new(entry.payload.as_slice())?;
            read_bounded(decoder, entry.uncompressed_length)?
        }
        PackEncoding::Delta => {
            let base_id = entry
                .base_id
                .ok_or(StoreError::CorruptPack("delta lacks a base"))?;
            let base_position = index
                .binary_search_by_key(&base_id, |candidate| candidate.id)
                .map_err(|_| StoreError::CorruptPack("delta base is absent from index"))?;
            if index[base_position].offset >= indexed.offset {
                return Err(StoreError::CorruptPack(
                    "delta base does not precede dependent",
                ));
            }
            let base = reconstruct_pack_object(
                pack_path,
                index,
                base_id,
                max_object_bytes,
                recursion_depth + 1,
                cache,
                depths,
            )?;
            let base_depth = depths
                .get(&base_id)
                .copied()
                .ok_or(StoreError::CorruptPack("delta base lacks verified depth"))?;
            if entry.delta_depth != base_depth + 1 || entry.delta_depth > MAX_DELTA_DEPTH {
                return Err(StoreError::CorruptPack("delta depth is inconsistent"));
            }
            let delta_limit = entry
                .uncompressed_length
                .checked_add(DELTA_HEADER_LENGTH as u64)
                .ok_or(StoreError::CorruptPack("delta bound overflow"))?;
            let decoder = zstd::stream::read::Decoder::new(entry.payload.as_slice())?;
            let delta = read_at_most(decoder, delta_limit)?;
            apply_delta(&base, &delta, entry.uncompressed_length)?
        }
    };
    verify_object_bytes(id, entry.kind, entry.uncompressed_length, &bytes)?;
    depths.insert(id, entry.delta_depth);
    cache.insert(id, bytes.clone());
    Ok(bytes)
}

fn verify_pack_files(
    pack_path: &Path,
    index_path: &Path,
    pack_checksum: [u8; 32],
    index_checksum: [u8; 32],
    generation: u64,
    max_object_bytes: u64,
    expected_object_count: u64,
) -> Result<(), StoreError> {
    if expected_object_count == 0 || expected_object_count > u64::from(MAX_SUPPORTED_PACK_OBJECTS) {
        return Err(StoreError::CorruptPack("pack object count exceeds bounds"));
    }
    if checksum_path(pack_path)? != pack_checksum {
        return Err(StoreError::CorruptPack("pack checksum mismatch"));
    }
    let mut pack = File::open(pack_path)?;
    let mut header = [0_u8; PACK_HEADER_LENGTH as usize];
    pack.read_exact(&mut header)?;
    if &header[0..8] != PACK_MAGIC {
        return Err(StoreError::CorruptPack("invalid pack magic"));
    }
    let stored_generation = u64::from_be_bytes(
        header[8..16]
            .try_into()
            .map_err(|_| StoreError::CorruptPack("invalid pack generation"))?,
    );
    if stored_generation != generation {
        return Err(StoreError::CorruptPack("pack generation mismatch"));
    }
    let index = read_pack_index(
        index_path,
        pack_checksum,
        index_checksum,
        u64::from(MAX_SUPPORTED_PACK_OBJECTS),
    )?;
    if index.len() as u64 != expected_object_count {
        return Err(StoreError::CorruptPack(
            "pack object count disagrees with index",
        ));
    }
    let mut physical = index.clone();
    physical.sort_unstable_by_key(|entry| entry.offset);
    let mut expected_offset = PACK_HEADER_LENGTH;
    for entry in &physical {
        if entry.offset != expected_offset {
            return Err(StoreError::CorruptPack("pack entries are not contiguous"));
        }
        expected_offset = expected_offset
            .checked_add(entry.record_length)
            .ok_or(StoreError::CorruptPack("pack length overflow"))?;
    }
    if expected_offset != pack.metadata()?.len() {
        return Err(StoreError::CorruptPack("pack has unindexed trailing bytes"));
    }
    let mut cache = HashMap::new();
    let mut depths = HashMap::new();
    for entry in &physical {
        reconstruct_pack_object(
            pack_path,
            &index,
            entry.id,
            max_object_bytes,
            0,
            &mut cache,
            &mut depths,
        )?;
    }
    Ok(())
}

fn read_bounded(reader: impl Read, expected_length: u64) -> Result<Vec<u8>, StoreError> {
    let read_limit = expected_length
        .checked_add(1)
        .ok_or(StoreError::CorruptMetadata("object length overflow"))?;
    let capacity = usize::try_from(expected_length.min(1024 * 1024))
        .map_err(|_| StoreError::CorruptMetadata("object length exceeds host limits"))?;
    let mut bytes = Vec::with_capacity(capacity);
    reader.take(read_limit).read_to_end(&mut bytes)?;
    let actual_length = bytes.len() as u64;
    if actual_length != expected_length {
        return Err(StoreError::LengthMismatch {
            expected: expected_length,
            actual: actual_length,
        });
    }
    Ok(bytes)
}

fn read_at_most(reader: impl Read, limit: u64) -> Result<Vec<u8>, StoreError> {
    let capacity = usize::try_from(limit.min(1024 * 1024))
        .map_err(|_| StoreError::CorruptMetadata("read bound exceeds host limits"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let read_limit = limit
        .checked_add(1)
        .ok_or(StoreError::CorruptPack("bounded read length overflow"))?;
    reader.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(StoreError::CorruptPack(
            "decoded pack payload exceeds bound",
        ));
    }
    Ok(bytes)
}

fn verify_object_bytes(
    id: ObjectId,
    kind: ObjectKind,
    expected_length: u64,
    bytes: &[u8],
) -> Result<(), StoreError> {
    if bytes.len() as u64 != expected_length {
        return Err(StoreError::LengthMismatch {
            expected: expected_length,
            actual: bytes.len() as u64,
        });
    }
    if ObjectId::for_bytes(kind, bytes) != id {
        return Err(StoreError::HashMismatch(id));
    }
    Ok(())
}

fn parse_checksum(value: &str) -> Result<[u8; 32], StoreError> {
    let id = value
        .parse::<ObjectId>()
        .map_err(|_| StoreError::CorruptMetadata("invalid BLAKE3 checksum"))?;
    Ok(*id.as_bytes())
}

fn loose_relative_path(id: ObjectId) -> PathBuf {
    let digest = id.digest_hex();
    PathBuf::from("objects")
        .join("loose")
        .join("b3")
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest)
}

fn path_to_database(path: &Path) -> Result<String, StoreError> {
    if !is_safe_relative_path(path) {
        return Err(StoreError::CorruptMetadata("unsafe object location"));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or(StoreError::CorruptMetadata("object path is not UTF-8"))
}

fn loose_database_path(value: &str) -> Result<PathBuf, StoreError> {
    let path = PathBuf::from(value);
    if !is_safe_relative_path(&path) || !path.starts_with("objects/loose/b3") {
        return Err(StoreError::CorruptMetadata("unsafe object location"));
    }
    Ok(path)
}

fn pack_database_path(value: &str, extension: &str) -> Result<PathBuf, StoreError> {
    let path = PathBuf::from(value);
    if !is_safe_relative_path(&path)
        || !path.starts_with("objects/packs")
        || path.extension().and_then(|value| value.to_str()) != extension.strip_prefix('.')
    {
        return Err(StoreError::CorruptMetadata("unsafe pack location"));
    }
    Ok(path)
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn validate_collection_name(name: &str) -> Result<(), StoreError> {
    if name.trim().is_empty() {
        Err(StoreError::InvalidConfig(
            "collection name must be non-empty",
        ))
    } else {
        Ok(())
    }
}

fn validate_preview_commit(preview: CollectionPreviewCommit<'_>) -> Result<(), StoreError> {
    let page_count = u64::try_from(preview.members.len())
        .map_err(|_| StoreError::InvalidConfig("collection preview is too large"))?;
    if preview
        .budget
        .maximum_pages()
        .is_some_and(|limit| page_count > limit.get())
    {
        return Err(StoreError::CollectionBudgetExceeded {
            resource: "pages",
            limit: preview
                .budget
                .maximum_pages()
                .expect("checked page maximum")
                .get(),
            estimated: page_count,
        });
    }
    if let (Some(limit), Some(predicted)) = (
        preview.budget.maximum_bytes(),
        preview.predicted_canonical_bytes,
    ) && predicted > limit.get()
    {
        return Err(StoreError::CollectionBudgetExceeded {
            resource: "bytes",
            limit: limit.get(),
            estimated: predicted,
        });
    }
    let mut unique_page_ids = HashSet::with_capacity(preview.members.len());
    for member in preview.members {
        if !unique_page_ids.insert(member.page_id) {
            return Err(StoreError::InvalidConfig(
                "resolved membership contains a duplicate page ID",
            ));
        }
        validate_inclusion_reason(preview.rule, member)?;
    }
    let mut unique_missing_titles = HashSet::with_capacity(preview.missing_titles.len());
    if preview
        .missing_titles
        .iter()
        .any(|title| !unique_missing_titles.insert(title))
    {
        return Err(StoreError::InvalidConfig(
            "unresolved title preview contains a duplicate title",
        ));
    }
    Ok(())
}

fn commit_preview_transaction(
    transaction: &Transaction<'_>,
    raw_collection_id: i64,
    raw_wiki_id: i64,
    preview: CollectionPreviewCommit<'_>,
    image_policy: ImagePolicy,
    now: i64,
) -> Result<MembershipCommit, StoreError> {
    let (rule_kind, category_title, category_depth) = collection_rule_values(preview.rule);
    let (history_kind, history_value) = history_policy_values(preview.history_policy)?;
    let maximum_pages = preview
        .budget
        .maximum_pages()
        .map(|value| to_sql_integer(value.get()))
        .transpose()?;
    let maximum_bytes = preview
        .budget
        .maximum_bytes()
        .map(|value| to_sql_integer(value.get()))
        .transpose()?;
    let (image_kind, thumbnail_edge, thumbnail_count, thumbnail_bytes) =
        image_policy_values(image_policy);
    transaction.execute(
        "INSERT INTO collection_configuration (
            collection_id, rule_kind, category_title, category_recursion_depth,
            history_kind, history_value, maximum_pages, maximum_bytes,
            removal_policy, image_policy, thumbnail_max_edge_pixels,
            thumbnail_max_images_per_revision, thumbnail_max_bytes_per_image,
            updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                   ?11, ?12, ?13, ?14)
         ON CONFLICT(collection_id) DO UPDATE SET
            rule_kind = excluded.rule_kind,
            category_title = excluded.category_title,
            category_recursion_depth = excluded.category_recursion_depth,
            history_kind = excluded.history_kind,
            history_value = excluded.history_value,
            maximum_pages = excluded.maximum_pages,
            maximum_bytes = excluded.maximum_bytes,
            removal_policy = excluded.removal_policy,
            image_policy = excluded.image_policy,
            thumbnail_max_edge_pixels = excluded.thumbnail_max_edge_pixels,
            thumbnail_max_images_per_revision = excluded.thumbnail_max_images_per_revision,
            thumbnail_max_bytes_per_image = excluded.thumbnail_max_bytes_per_image,
            updated_at = excluded.updated_at",
        params![
            raw_collection_id,
            rule_kind,
            category_title,
            category_depth,
            history_kind,
            history_value,
            maximum_pages,
            maximum_bytes,
            removal_policy_value(preview.removal_policy),
            image_kind,
            thumbnail_edge,
            thumbnail_count,
            thumbnail_bytes,
            now,
        ],
    )?;
    transaction.execute(
        "DELETE FROM collection_rule_titles WHERE collection_id = ?1",
        [raw_collection_id],
    )?;
    if let Some(titles) = preview.rule.titles() {
        for title in titles.iter() {
            transaction.execute(
                "INSERT INTO collection_rule_titles (collection_id, title) VALUES (?1, ?2)",
                params![raw_collection_id, title.as_str()],
            )?;
        }
    }
    transaction.execute(
        "DELETE FROM unresolved_titles WHERE collection_id = ?1",
        [raw_collection_id],
    )?;
    for title in preview.missing_titles {
        transaction.execute(
            "INSERT INTO unresolved_titles (
                collection_id, title, namespace, last_observed_at
             ) VALUES (?1, ?2, 0, ?3)",
            params![raw_collection_id, title.as_str(), now],
        )?;
    }

    if preview.removal_policy == CollectionRemovalPolicy::StopTrackingRetainHistory {
        transaction.execute(
            "UPDATE collection_resolved_members
             SET membership_state = 'removed', removed_at = ?2
             WHERE collection_id = ?1 AND membership_state = 'active'",
            params![raw_collection_id, now],
        )?;
        transaction.execute(
            "DELETE FROM collection_pages WHERE collection_id = ?1",
            [raw_collection_id],
        )?;
    }
    for member in preview.members {
        let (kind, inclusion_title, inclusion_depth) =
            inclusion_reason_values(&member.inclusion_reason);
        let raw_page_id = to_sql_integer(member.page_id.get())?;
        transaction.execute(
            "INSERT INTO collection_resolved_members (
                collection_id, wiki_id, page_id, namespace, title,
                inclusion_kind, inclusion_title, inclusion_depth,
                membership_state, first_resolved_at, last_resolved_at, removed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                       'active', ?9, ?9, NULL)
             ON CONFLICT(collection_id, page_id) DO UPDATE SET
                wiki_id = excluded.wiki_id,
                namespace = excluded.namespace,
                title = excluded.title,
                inclusion_kind = excluded.inclusion_kind,
                inclusion_title = excluded.inclusion_title,
                inclusion_depth = excluded.inclusion_depth,
                membership_state = 'active',
                last_resolved_at = excluded.last_resolved_at,
                removed_at = NULL",
            params![
                raw_collection_id,
                raw_wiki_id,
                raw_page_id,
                member.namespace,
                member.title.as_str(),
                kind,
                inclusion_title,
                inclusion_depth,
                now,
            ],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO collection_pages (
                collection_id, wiki_id, page_id, inclusion_reason, added_at
             )
             SELECT ?1, ?2, ?3, 'explicit-title', ?4
             WHERE EXISTS (
                SELECT 1 FROM pages WHERE wiki_id = ?2 AND page_id = ?3
             )",
            params![raw_collection_id, raw_wiki_id, raw_page_id, now],
        )?;
    }
    let active_members: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM collection_resolved_members
         WHERE collection_id = ?1 AND membership_state = 'active'",
        [raw_collection_id],
        |row| row.get(0),
    )?;
    let active_members = sql_u64(active_members, "invalid active member count")?;
    if preview
        .budget
        .maximum_pages()
        .is_some_and(|limit| active_members > limit.get())
    {
        return Err(StoreError::CollectionBudgetExceeded {
            resource: "pages",
            limit: preview
                .budget
                .maximum_pages()
                .expect("checked page maximum")
                .get(),
            estimated: active_members,
        });
    }
    let current_canonical_bytes: i64 = transaction.query_row(
        "SELECT COALESCE(SUM(uncompressed_length), 0)
         FROM content_objects
         WHERE object_id IN (
            SELECT DISTINCT revisions.content_object_id
            FROM collection_resolved_members AS members
            JOIN revisions
              ON revisions.wiki_id = members.wiki_id
             AND revisions.page_id = members.page_id
            WHERE members.collection_id = ?1
              AND members.membership_state = 'active'
            UNION
            SELECT placements.content_object_id
            FROM collection_resolved_members AS members
            JOIN revisions
              ON revisions.wiki_id = members.wiki_id
             AND revisions.page_id = members.page_id
            JOIN page_media AS placements
              ON placements.wiki_id = revisions.wiki_id
             AND placements.revision_id = revisions.revision_id
            WHERE members.collection_id = ?1
              AND members.membership_state = 'active'
         )",
        [raw_collection_id],
        |row| row.get(0),
    )?;
    let current_canonical_bytes = sql_u64(
        current_canonical_bytes,
        "invalid collection canonical byte count",
    )?;
    let expected_bytes = preview
        .predicted_canonical_bytes
        .unwrap_or_default()
        .max(current_canonical_bytes);
    if preview
        .budget
        .maximum_bytes()
        .is_some_and(|limit| expected_bytes > limit.get())
    {
        return Err(StoreError::CollectionBudgetExceeded {
            resource: "bytes",
            limit: preview
                .budget
                .maximum_bytes()
                .expect("checked byte maximum")
                .get(),
            estimated: expected_bytes,
        });
    }
    transaction.execute(
        "INSERT INTO collection_estimates (
            collection_id, resolved_page_count, predicted_canonical_bytes, estimated_at
         ) VALUES (?1, ?2, ?3, ?4)",
        params![
            raw_collection_id,
            to_sql_integer(active_members)?,
            preview
                .predicted_canonical_bytes
                .map(to_sql_integer)
                .transpose()?,
            now,
        ],
    )?;
    let removed_members: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM collection_resolved_members
         WHERE collection_id = ?1 AND membership_state = 'removed' AND removed_at = ?2",
        params![raw_collection_id, now],
        |row| row.get(0),
    )?;
    Ok(MembershipCommit {
        active_members,
        removed_members: sql_u64(removed_members, "invalid removed member count")?,
    })
}

fn collection_rule_values(rule: &CollectionRule) -> (&'static str, Option<&str>, Option<i64>) {
    match rule {
        CollectionRule::ExplicitTitles(_) => ("explicit-titles", None, None),
        CollectionRule::TitleList(_) => ("title-list", None, None),
        CollectionRule::Category {
            title,
            recursion_depth,
        } => (
            "category",
            Some(title.as_str()),
            Some(i64::from(*recursion_depth)),
        ),
    }
}

fn history_policy_values(policy: HistoryPolicy) -> Result<(&'static str, Option<i64>), StoreError> {
    match policy {
        HistoryPolicy::CurrentAndFuture => Ok(("current-and-future", None)),
        HistoryPolicy::LastN(count) => Ok(("last-n", Some(i64::from(count.get())))),
        HistoryPolicy::Since(timestamp) => Ok(("since", Some(timestamp.as_seconds()))),
        HistoryPolicy::Complete => Ok(("complete", None)),
    }
}

fn stored_history_policy(kind: &str, value: Option<i64>) -> Result<HistoryPolicy, StoreError> {
    match kind {
        "current-and-future" if value.is_none() => Ok(HistoryPolicy::CurrentAndFuture),
        "last-n" => HistoryPolicy::last_n(
            u32::try_from(value.ok_or(StoreError::CorruptMetadata("last-N policy lacks value"))?)
                .map_err(|_| StoreError::CorruptMetadata("invalid last-N history value"))?,
        )
        .map_err(|_| StoreError::CorruptMetadata("invalid last-N history value")),
        "since" => Ok(HistoryPolicy::Since(UnixTimestamp::from_seconds(
            value.ok_or(StoreError::CorruptMetadata("since policy lacks value"))?,
        ))),
        "complete" if value.is_none() => Ok(HistoryPolicy::Complete),
        _ => Err(StoreError::CorruptMetadata("invalid history policy")),
    }
}

fn stored_collection_budget(
    maximum_pages: Option<i64>,
    maximum_bytes: Option<i64>,
) -> Result<CollectionBudget, StoreError> {
    let mut budget = CollectionBudget::unlimited();
    if let Some(pages) = maximum_pages {
        budget = budget
            .with_maximum_pages(sql_u64(pages, "invalid collection page budget")?)
            .map_err(|_| StoreError::CorruptMetadata("invalid collection page budget"))?;
    }
    if let Some(bytes) = maximum_bytes {
        budget = budget
            .with_maximum_bytes(sql_u64(bytes, "invalid collection byte budget")?)
            .map_err(|_| StoreError::CorruptMetadata("invalid collection byte budget"))?;
    }
    Ok(budget)
}

fn image_policy_values(
    policy: ImagePolicy,
) -> (&'static str, Option<i64>, Option<i64>, Option<i64>) {
    match policy {
        ImagePolicy::None => ("none", None, None, None),
        ImagePolicy::Thumbnails(policy) => (
            "thumbnails",
            Some(i64::from(policy.maximum_edge_pixels().get())),
            Some(i64::from(policy.maximum_images_per_revision().get())),
            Some(policy.maximum_bytes_per_image().get() as i64),
        ),
    }
}

fn stored_image_policy(
    kind: &str,
    maximum_edge_pixels: Option<i64>,
    maximum_images_per_revision: Option<i64>,
    maximum_bytes_per_image: Option<i64>,
) -> Result<ImagePolicy, StoreError> {
    match (
        kind,
        maximum_edge_pixels,
        maximum_images_per_revision,
        maximum_bytes_per_image,
    ) {
        ("none", None, None, None) => Ok(ImagePolicy::None),
        ("thumbnails", Some(edge), Some(images), Some(bytes)) => {
            let policy = ThumbnailPolicy::new(
                u32::try_from(edge)
                    .map_err(|_| StoreError::CorruptMetadata("invalid thumbnail edge bound"))?,
                u32::try_from(images)
                    .map_err(|_| StoreError::CorruptMetadata("invalid thumbnail count bound"))?,
                sql_u64(bytes, "invalid thumbnail byte bound")?,
            )
            .map_err(|_| StoreError::CorruptMetadata("invalid thumbnail policy bounds"))?;
            Ok(ImagePolicy::Thumbnails(policy))
        }
        _ => Err(StoreError::CorruptMetadata(
            "invalid stored collection image policy",
        )),
    }
}

fn stored_collection_image_policy(
    connection: &Connection,
    collection_id: CollectionId,
    raw_collection_id: i64,
) -> Result<ImagePolicy, StoreError> {
    let row = connection
        .query_row(
            "SELECT image_policy, thumbnail_max_edge_pixels,
                    thumbnail_max_images_per_revision, thumbnail_max_bytes_per_image
             FROM collection_configuration WHERE collection_id = ?1",
            [raw_collection_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
    stored_image_policy(&row.0, row.1, row.2, row.3)
}

const fn removal_policy_value(policy: CollectionRemovalPolicy) -> &'static str {
    match policy {
        CollectionRemovalPolicy::StopTrackingRetainHistory => "stop-tracking-retain-history",
        CollectionRemovalPolicy::KeepTracking => "keep-tracking",
    }
}

fn stored_removal_policy(value: &str) -> Result<CollectionRemovalPolicy, StoreError> {
    match value {
        "stop-tracking-retain-history" => Ok(CollectionRemovalPolicy::StopTrackingRetainHistory),
        "keep-tracking" => Ok(CollectionRemovalPolicy::KeepTracking),
        _ => Err(StoreError::CorruptMetadata(
            "unknown collection removal policy",
        )),
    }
}

fn inclusion_reason_values(reason: &InclusionReason) -> (&'static str, &str, Option<i64>) {
    match reason {
        InclusionReason::ExplicitTitle(title) => ("explicit-title", title.as_str(), None),
        InclusionReason::TitleList(title) => ("title-list", title.as_str(), None),
        InclusionReason::Category { category, depth } => {
            ("category", category.as_str(), Some(i64::from(*depth)))
        }
    }
}

fn stored_inclusion_reason(
    kind: &str,
    title: String,
    depth: Option<i64>,
) -> Result<InclusionReason, StoreError> {
    let title = PageTitle::new(title)
        .map_err(|_| StoreError::CorruptMetadata("invalid inclusion reason title"))?;
    match kind {
        "explicit-title" if depth.is_none() => Ok(InclusionReason::ExplicitTitle(title)),
        "title-list" if depth.is_none() => Ok(InclusionReason::TitleList(title)),
        "category" => Ok(InclusionReason::Category {
            category: title,
            depth: u16::try_from(
                depth.ok_or(StoreError::CorruptMetadata("category reason lacks depth"))?,
            )
            .map_err(|_| StoreError::CorruptMetadata("invalid category inclusion depth"))?,
        }),
        _ => Err(StoreError::CorruptMetadata("invalid inclusion reason")),
    }
}

fn validate_inclusion_reason(
    rule: &CollectionRule,
    member: &ResolvedCollectionMember,
) -> Result<(), StoreError> {
    let valid = match (rule, &member.inclusion_reason) {
        // MediaWiki may normalize or redirect a configured title before returning
        // the stable page identity. The configured titles remain persisted in the
        // rule while the inclusion reason records the canonical resolved title.
        (CollectionRule::ExplicitTitles(_), InclusionReason::ExplicitTitle(_))
        | (CollectionRule::TitleList(_), InclusionReason::TitleList(_)) => true,
        (
            CollectionRule::Category {
                title,
                recursion_depth,
            },
            InclusionReason::Category { category, depth },
        ) => category == title && depth <= recursion_depth,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidInclusionReason(member.page_id))
    }
}

fn schedule_cadence_values(cadence: ScheduleCadence) -> (&'static str, Option<u32>) {
    match cadence {
        ScheduleCadence::Manual => ("manual", None),
        ScheduleCadence::Interval(interval) => ("interval", Some(interval.seconds())),
        ScheduleCadence::DailyUtc(time) => ("daily-utc", Some(time.seconds_after_midnight())),
    }
}

fn validate_schedule_configuration(
    cadence: ScheduleCadence,
    jitter_seconds: u32,
    next_run_at: Option<u64>,
) -> Result<(), StoreError> {
    if jitter_seconds > MAX_SCHEDULE_JITTER_SECONDS {
        return Err(StoreError::InvalidConfig(
            "schedule jitter must not exceed 86,400 seconds",
        ));
    }
    match cadence {
        ScheduleCadence::Manual if jitter_seconds != 0 || next_run_at.is_some() => Err(
            StoreError::InvalidConfig("manual schedule must not have jitter or a next run"),
        ),
        ScheduleCadence::Manual => Ok(()),
        ScheduleCadence::Interval(interval) if jitter_seconds > interval.seconds() => Err(
            StoreError::InvalidConfig("schedule jitter must not exceed its interval"),
        ),
        ScheduleCadence::Interval(_) | ScheduleCadence::DailyUtc(_) if next_run_at.is_none() => {
            Err(StoreError::InvalidConfig(
                "recurring schedule requires a next run time",
            ))
        }
        ScheduleCadence::Interval(_) | ScheduleCadence::DailyUtc(_) => Ok(()),
    }
}

fn read_network_transfer_policy(
    connection: &Connection,
) -> Result<NetworkTransferPolicy, StoreError> {
    let row = connection
        .query_row(
            "SELECT max_concurrent_requests, max_download_bytes_per_second,
                    avoid_metered_networks
             FROM network_transfer_policy WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::CorruptMetadata(
            "network transfer policy row is missing",
        ))?;
    let max_concurrent_requests = u32::try_from(row.0)
        .map_err(|_| StoreError::CorruptMetadata("invalid maximum concurrent request policy"))?;
    let max_download_bytes_per_second = row
        .1
        .map(|value| {
            u64::try_from(value)
                .map_err(|_| StoreError::CorruptMetadata("invalid maximum download rate policy"))
        })
        .transpose()?;
    let avoid_metered_networks = match row.2 {
        0 => false,
        1 => true,
        _ => {
            return Err(StoreError::CorruptMetadata(
                "invalid metered-network avoidance policy",
            ));
        }
    };
    NetworkTransferPolicy::new(
        max_concurrent_requests,
        max_download_bytes_per_second,
        avoid_metered_networks,
    )
    .map_err(|_| StoreError::CorruptMetadata("network transfer policy is out of range"))
}

fn migrate(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY NOT NULL CHECK (version > 0),
            name TEXT NOT NULL,
            applied_at INTEGER NOT NULL
         ) STRICT;",
    )?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > 14 {
        return Err(StoreError::UnsupportedSchemaVersion(version));
    }
    if version == 0 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_1)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (1, 'initial', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 1)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 1 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_2)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (2, 'capture', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 2)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 2 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_3)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (3, 'search', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 3)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 3 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_4)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (4, 'sync', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 4)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 4 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_5)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (5, 'packs', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 5)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 5 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_6)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (6, 'collections', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 6)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 6 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_7)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (7, 'schedules', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 7)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 7 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_8)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (8, 'manifest-configuration', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 8)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 8 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_9)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (9, 'network-transfer-policy', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 9)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 9 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_10)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (10, 'collection-status', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 10)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 10 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_11)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (11, 'pack-affinity', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 11)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 11 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_12)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (12, 'thumbnail-media', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 12)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 12 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_13)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (13, 'dump-imports', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 13)?;
        transaction.commit()?;
    }
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 13 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_14)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (14, 'purge-journal', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 14)?;
        transaction.commit()?;
    }
    Ok(())
}

fn sql_id<T>(value: i64, message: &'static str) -> Result<T, StoreError>
where
    T: TryFrom<u64>,
{
    let value = u64::try_from(value).map_err(|_| StoreError::CorruptMetadata(message))?;
    T::try_from(value).map_err(|_| StoreError::CorruptMetadata(message))
}

fn sql_u64(value: i64, message: &'static str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::CorruptMetadata(message))
}

fn stored_collection_status(value: &str) -> Result<CollectionStatus, StoreError> {
    match value {
        "active" => Ok(CollectionStatus::Active),
        "tombstoned" => Ok(CollectionStatus::Tombstoned),
        _ => Err(StoreError::CorruptMetadata("unknown collection status")),
    }
}

fn collection_status(
    connection: &Connection,
    collection_id: CollectionId,
    raw_collection_id: i64,
) -> Result<CollectionStatus, StoreError> {
    let status = connection
        .query_row(
            "SELECT status FROM collections WHERE collection_id = ?1",
            [raw_collection_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    status
        .as_deref()
        .map(stored_collection_status)
        .transpose()?
        .ok_or(StoreError::CollectionNotFound(collection_id))
}

fn ensure_collection_active(
    connection: &Connection,
    collection_id: CollectionId,
    raw_collection_id: i64,
) -> Result<(), StoreError> {
    match collection_status(connection, collection_id, raw_collection_id)? {
        CollectionStatus::Active => Ok(()),
        CollectionStatus::Tombstoned => Err(StoreError::CollectionTombstoned(collection_id)),
    }
}

fn purge_manifest_binding(
    manifests: &[StoredManifest],
    collection_id: CollectionId,
) -> (Option<(u64, ManifestId)>, HashSet<ObjectId>) {
    let head = manifests
        .last()
        .map(|stored| (stored.manifest.sequence, stored.id));
    let mut revision_objects = HashMap::new();
    for stored in manifests {
        for revision in &stored.manifest.introduced_revisions {
            revision_objects.insert(
                (stored.manifest.wiki_id, revision.revision_id),
                revision.content_object_id,
            );
        }
    }
    let mut protected = HashSet::new();
    for stored in manifests {
        if stored.manifest.collection_id == Some(collection_id) {
            continue;
        }
        for revision in &stored.manifest.introduced_revisions {
            protected.insert(revision.content_object_id);
        }
        for head in &stored.manifest.page_heads {
            if let Some(revision_id) = head.revision_id
                && let Some(object_id) =
                    revision_objects.get(&(stored.manifest.wiki_id, revision_id))
            {
                protected.insert(*object_id);
            }
        }
        if let Some(snapshot) = &stored.manifest.media_snapshot {
            protected.extend(
                snapshot
                    .inventory
                    .iter()
                    .map(|media| media.content_object_id),
            );
            protected.extend(
                snapshot
                    .placements
                    .iter()
                    .map(|placement| placement.content_object_id),
            );
        }
    }
    (head, protected)
}

fn compute_purge_preview(
    connection: &Connection,
    collection_id: CollectionId,
    manifest_head: Option<(u64, ManifestId)>,
    protected_manifest_objects: &HashSet<ObjectId>,
) -> Result<(PurgePreview, Vec<PurgeCandidate>, Vec<PurgePackSnapshot>), StoreError> {
    let raw_collection_id = to_sql_integer(collection_id.get())?;
    let collection = connection
        .query_row(
            "SELECT name, generation, status, tombstoned_at
             FROM collections WHERE collection_id = ?1",
            [raw_collection_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::CollectionNotFound(collection_id))?;
    if stored_collection_status(&collection.2)? != CollectionStatus::Tombstoned {
        return Err(StoreError::CollectionMustBeTombstoned(collection_id));
    }
    let tombstoned_at = collection.3.ok_or(StoreError::CorruptMetadata(
        "tombstoned collection lacks tombstone time",
    ))?;
    validate_purge_text(&collection.0)?;

    // Pages remain target-exclusive only when no other retained collection membership
    // row names the same stable wiki/page identity. Removed membership is intentional
    // audit scope and therefore participates on both sides of this closure.
    let mut statement = connection.prepare(
        "WITH target_pages AS (
             SELECT wiki_id, page_id
             FROM collection_resolved_members
             WHERE collection_id = ?1
         ), exclusive_pages AS (
             SELECT target.wiki_id, target.page_id
             FROM target_pages AS target
             WHERE NOT EXISTS (
                 SELECT 1 FROM collection_resolved_members AS other
                 WHERE other.wiki_id = target.wiki_id
                   AND other.page_id = target.page_id
                   AND other.collection_id != ?1
             )
         ), seeds(object_id) AS (
             SELECT revision.content_object_id
             FROM revisions AS revision
             JOIN exclusive_pages AS target
               ON target.wiki_id = revision.wiki_id
              AND target.page_id = revision.page_id
             UNION
             SELECT placement.content_object_id
             FROM page_media AS placement
             JOIN revisions AS revision
               ON revision.wiki_id = placement.wiki_id
              AND revision.revision_id = placement.revision_id
             JOIN exclusive_pages AS target
               ON target.wiki_id = revision.wiki_id
              AND target.page_id = revision.page_id
         )
         SELECT object.object_id, object.object_kind, object.uncompressed_length
         FROM seeds
         JOIN content_objects AS object USING (object_id)
         WHERE object.verification_state = 'verified'
         AND ((
             object.object_kind = 'wikitext'
             AND NOT EXISTS (
                 SELECT 1 FROM revisions AS retained
                 WHERE retained.content_object_id = object.object_id
                   AND NOT EXISTS (
                       SELECT 1 FROM exclusive_pages AS target
                       WHERE target.wiki_id = retained.wiki_id
                         AND target.page_id = retained.page_id
                   )
             )
         ) OR (
             object.object_kind = 'media'
             AND NOT EXISTS (
                 SELECT 1
                 FROM page_media AS retained_placement
                 JOIN revisions AS retained_revision
                   ON retained_revision.wiki_id = retained_placement.wiki_id
                  AND retained_revision.revision_id = retained_placement.revision_id
                 WHERE retained_placement.content_object_id = object.object_id
                   AND NOT EXISTS (
                       SELECT 1 FROM exclusive_pages AS target
                       WHERE target.wiki_id = retained_revision.wiki_id
                         AND target.page_id = retained_revision.page_id
                   )
             )
             AND NOT EXISTS (
                 SELECT 1 FROM media AS catalog
                 WHERE catalog.content_object_id = object.object_id
                   AND NOT EXISTS (
                       SELECT 1
                       FROM page_media AS target_placement
                       JOIN revisions AS target_revision
                         ON target_revision.wiki_id = target_placement.wiki_id
                        AND target_revision.revision_id = target_placement.revision_id
                       JOIN exclusive_pages AS target
                         ON target.wiki_id = target_revision.wiki_id
                        AND target.page_id = target_revision.page_id
                       WHERE target_placement.wiki_id = catalog.wiki_id
                         AND target_placement.source_media_id = catalog.source_media_id
                         AND target_placement.source_sha1 = catalog.source_sha1
                         AND target_placement.content_object_id = catalog.content_object_id
                   )
             )
         ))
         ORDER BY object.object_id
         LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![raw_collection_id, i64::from(MAX_PURGE_OBJECTS) + 1],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    let mut candidates = Vec::new();
    for row in rows {
        let (id, kind, length) = row?;
        candidates.push(PurgeCandidate {
            id: id
                .parse()
                .map_err(|_| StoreError::CorruptMetadata("invalid purge candidate ID"))?,
            kind: ObjectKind::from_database(&kind)?,
            uncompressed_length: sql_u64(length, "invalid purge candidate length")?,
        });
    }
    if candidates.len() > MAX_PURGE_OBJECTS as usize {
        return Err(StoreError::PurgeLimitExceeded);
    }
    candidates.retain(|candidate| !protected_manifest_objects.contains(&candidate.id));
    if candidates.is_empty() {
        return Err(StoreError::NoExclusivePurgePayload(collection_id));
    }

    let mut locations = Vec::new();
    let mut affected_packs: BTreeMap<String, HashSet<ObjectId>> = BTreeMap::new();
    let mut loose_object_count = 0_u64;
    let mut reclaimable_bytes = 0_u64;
    let mut total_location_count = 0_u64;
    let mut location_statement = connection.prepare(
        "SELECT location.storage_kind, location.encoding, location.relative_path,
                location.compressed_length, location.base_object_id,
                location.pack_generation, location.pack_id, location.pack_offset,
                location.delta_depth, pack.index_checksum
         FROM object_locations AS location
         LEFT JOIN packs AS pack ON pack.pack_id = location.pack_id
         WHERE location.object_id = ?1
           AND location.verification_state = 'verified'
           AND (location.storage_kind = 'loose' OR pack.state = 'verified')
         ORDER BY location.storage_kind, location.relative_path, location.location_id",
    )?;
    for candidate in &candidates {
        let rows = location_statement.query_map([candidate.id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })?;
        let mut candidate_location_count = 0_u64;
        let mut has_loose = false;
        for row in rows {
            let (
                storage_kind,
                encoding,
                relative_path,
                compressed_length,
                base_object_id,
                pack_generation,
                pack_id,
                pack_offset,
                delta_depth,
                pack_index_checksum,
            ) = row?;
            let compressed_length = sql_u64(compressed_length, "invalid purge location length")?;
            if storage_kind == "loose" {
                has_loose = true;
                reclaimable_bytes = reclaimable_bytes
                    .checked_add(compressed_length)
                    .ok_or(StoreError::PurgeLimitExceeded)?;
            } else if let Some(pack_id) = &pack_id {
                affected_packs
                    .entry(pack_id.clone())
                    .or_default()
                    .insert(candidate.id);
            }
            locations.push(PurgeLocationFingerprint {
                object_id: candidate.id,
                storage_kind,
                encoding,
                relative_path,
                compressed_length,
                base_object_id: base_object_id
                    .map(|id| {
                        id.parse()
                            .map_err(|_| StoreError::CorruptMetadata("invalid purge delta base ID"))
                    })
                    .transpose()?,
                pack_generation: pack_generation
                    .map(|value| sql_u64(value, "invalid purge pack generation"))
                    .transpose()?,
                pack_id,
                pack_index_checksum,
                pack_offset: pack_offset
                    .map(|value| sql_u64(value, "invalid purge pack offset"))
                    .transpose()?,
                delta_depth: delta_depth
                    .map(|value| {
                        u16::try_from(value)
                            .map_err(|_| StoreError::CorruptMetadata("invalid purge delta depth"))
                    })
                    .transpose()?,
            });
            candidate_location_count += 1;
            total_location_count += 1;
            if total_location_count > u64::from(MAX_PURGE_LOCATIONS) {
                return Err(StoreError::PurgeLocationLimitExceeded);
            }
        }
        if candidate_location_count == 0 {
            return Err(StoreError::PurgeObjectUnavailable(candidate.id));
        }
        if has_loose {
            loose_object_count += 1;
        }
    }
    locations.sort();

    let mut pack_snapshots = Vec::with_capacity(affected_packs.len());
    if affected_packs.len() > MAX_PURGE_AFFECTED_PACKS as usize {
        return Err(StoreError::PurgePackLimitExceeded);
    }
    for (pack_id, purged_ids) in affected_packs {
        let (object_count, location_count, record_bytes): (i64, i64, i64) = connection.query_row(
            "SELECT pack.object_count,
                        COUNT(location.location_id),
                        COALESCE(SUM(location.compressed_length), 0)
                 FROM packs AS pack
                 LEFT JOIN object_locations AS location
                   ON location.pack_id = pack.pack_id
                  AND location.verification_state = 'verified'
                 WHERE pack.pack_id = ?1 AND pack.state = 'verified'
                 GROUP BY pack.pack_id",
            [&pack_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let object_count = sql_u64(object_count, "invalid purge pack object count")?;
        let location_count = sql_u64(location_count, "invalid purge pack location count")?;
        if object_count != location_count || purged_ids.len() as u64 > object_count {
            return Err(StoreError::CorruptMetadata(
                "purge pack object count disagrees with locations",
            ));
        }
        let purged_object_count = purged_ids.len() as u64;
        let retained_object_count = object_count - purged_object_count;
        let pack_reclaimable = if retained_object_count == 0 {
            PACK_HEADER_LENGTH
                .checked_add(sql_u64(record_bytes, "invalid purge pack byte count")?)
                .and_then(|bytes| bytes.checked_add(INDEX_HEADER_LENGTH))
                .and_then(|bytes| {
                    INDEX_ENTRY_LENGTH
                        .checked_mul(object_count)
                        .and_then(|index| bytes.checked_add(index))
                })
                .ok_or(StoreError::PurgeLimitExceeded)?
        } else {
            0
        };
        reclaimable_bytes = reclaimable_bytes
            .checked_add(pack_reclaimable)
            .ok_or(StoreError::PurgeLimitExceeded)?;
        pack_snapshots.push(PurgePackSnapshot {
            pack_id,
            purged_object_count,
            retained_object_count,
            reclaimable_bytes: pack_reclaimable,
        });
    }

    let wikitext_object_count = candidates
        .iter()
        .filter(|candidate| candidate.kind == ObjectKind::Wikitext)
        .count() as u64;
    let media_object_count = candidates.len() as u64 - wikitext_object_count;
    let logical_bytes = candidates.iter().try_fold(0_u64, |total, candidate| {
        total
            .checked_add(candidate.uncompressed_length)
            .ok_or(StoreError::PurgeLimitExceeded)
    })?;
    let whole_pack_count = pack_snapshots
        .iter()
        .filter(|pack| pack.retained_object_count == 0)
        .count() as u64;
    let affected_pack_count = pack_snapshots.len() as u64;
    let mixed_pack_count = affected_pack_count - whole_pack_count;
    let (manifest_head_sequence, manifest_head_id) = manifest_head.unzip();

    let mut preview = PurgePreview {
        collection_id,
        collection_name: collection.0,
        collection_generation: sql_u64(collection.1, "invalid purge collection generation")?,
        tombstoned_at: sql_u64(tombstoned_at, "invalid purge tombstone time")?,
        manifest_head_sequence,
        manifest_head_id,
        fingerprint: String::new(),
        object_count: candidates.len() as u64,
        wikitext_object_count,
        media_object_count,
        logical_bytes,
        reclaimable_bytes,
        loose_object_count,
        affected_pack_count,
        whole_pack_count,
        mixed_pack_count,
    };
    preview.fingerprint =
        purge_preview_fingerprint(&preview, &candidates, &locations, &pack_snapshots);
    Ok((preview, candidates, pack_snapshots))
}

fn purge_preview_fingerprint(
    preview: &PurgePreview,
    candidates: &[PurgeCandidate],
    locations: &[PurgeLocationFingerprint],
    packs: &[PurgePackSnapshot],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PURGE_PREVIEW_DOMAIN);
    hash_manifest_field(&mut hasher, &preview.collection_id.get().to_string());
    hash_manifest_field(&mut hasher, &preview.collection_name);
    hash_manifest_field(&mut hasher, &preview.collection_generation.to_string());
    hash_manifest_field(&mut hasher, &preview.tombstoned_at.to_string());
    hash_manifest_optional_field(
        &mut hasher,
        preview
            .manifest_head_sequence
            .map(|value| value.to_string())
            .as_deref(),
    );
    hash_manifest_optional_field(
        &mut hasher,
        preview.manifest_head_id.map(|id| id.to_string()).as_deref(),
    );
    for candidate in candidates {
        hash_manifest_field(&mut hasher, &candidate.id.to_string());
        hash_manifest_field(&mut hasher, candidate.kind.database_value());
        hash_manifest_field(&mut hasher, &candidate.uncompressed_length.to_string());
        hash_manifest_field(&mut hasher, "verified");
    }
    for location in locations {
        hash_manifest_field(&mut hasher, &location.object_id.to_string());
        hash_manifest_field(&mut hasher, &location.storage_kind);
        hash_manifest_field(&mut hasher, &location.encoding);
        hash_manifest_field(&mut hasher, &location.relative_path);
        hash_manifest_field(&mut hasher, &location.compressed_length.to_string());
        hash_manifest_optional_field(
            &mut hasher,
            location.base_object_id.map(|id| id.to_string()).as_deref(),
        );
        hash_manifest_optional_field(
            &mut hasher,
            location
                .pack_generation
                .map(|value| value.to_string())
                .as_deref(),
        );
        hash_manifest_optional_field(&mut hasher, location.pack_id.as_deref());
        hash_manifest_optional_field(&mut hasher, location.pack_index_checksum.as_deref());
        hash_manifest_optional_field(
            &mut hasher,
            location
                .pack_offset
                .map(|value| value.to_string())
                .as_deref(),
        );
        hash_manifest_optional_field(
            &mut hasher,
            location
                .delta_depth
                .map(|value| value.to_string())
                .as_deref(),
        );
    }
    for pack in packs {
        hash_manifest_field(&mut hasher, &pack.pack_id);
        hash_manifest_field(&mut hasher, &pack.purged_object_count.to_string());
        hash_manifest_field(&mut hasher, &pack.retained_object_count.to_string());
        hash_manifest_field(&mut hasher, &pack.reclaimable_bytes.to_string());
    }
    format!("b3:{}", hasher.finalize().to_hex())
}

fn authorized_purge_for_collection(
    connection: &Connection,
    collection_id: CollectionId,
) -> Result<Option<AuthorizedPurge>, StoreError> {
    type Row = (
        i64,
        String,
        i64,
        i64,
        Option<i64>,
        Option<String>,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    );
    let row: Option<Row> = connection
        .query_row(
            "SELECT purge_id, collection_name, collection_generation, tombstoned_at,
                    manifest_head_sequence, manifest_head_id, preview_fingerprint,
                    object_count, wikitext_object_count, media_object_count,
                    logical_bytes, reclaimable_bytes, loose_object_count,
                    affected_pack_count, whole_pack_count, mixed_pack_count, authorized_at
             FROM purge_operations
             WHERE collection_id = ?1
               AND state IN ('authorized', 'repacking', 'cleaning')",
            [to_sql_integer(collection_id.get())?],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        let manifest_head_id = row
            .5
            .map(|id| {
                id.parse().map_err(|_| {
                    StoreError::CorruptMetadata("invalid purge manifest head identity")
                })
            })
            .transpose()?;
        Ok(AuthorizedPurge {
            purge_id: sql_u64(row.0, "invalid purge ID")?,
            preview: PurgePreview {
                collection_id,
                collection_name: row.1,
                collection_generation: sql_u64(row.2, "invalid purge collection generation")?,
                tombstoned_at: sql_u64(row.3, "invalid purge tombstone time")?,
                manifest_head_sequence: row
                    .4
                    .map(|value| sql_u64(value, "invalid purge manifest sequence"))
                    .transpose()?,
                manifest_head_id,
                fingerprint: row.6,
                object_count: sql_u64(row.7, "invalid purge object count")?,
                wikitext_object_count: sql_u64(row.8, "invalid purge wikitext count")?,
                media_object_count: sql_u64(row.9, "invalid purge media count")?,
                logical_bytes: sql_u64(row.10, "invalid purge logical bytes")?,
                reclaimable_bytes: sql_u64(row.11, "invalid purge reclaimable bytes")?,
                loose_object_count: sql_u64(row.12, "invalid purge loose count")?,
                affected_pack_count: sql_u64(row.13, "invalid purge pack count")?,
                whole_pack_count: sql_u64(row.14, "invalid purge whole pack count")?,
                mixed_pack_count: sql_u64(row.15, "invalid purge mixed pack count")?,
            },
            authorized_at: sql_u64(row.16, "invalid purge authorization time")?,
        })
    })
    .transpose()
}

fn validate_purge_text(value: &str) -> Result<(), StoreError> {
    if value.trim().is_empty() || value.len() > MAX_MEDIA_METADATA_TEXT_BYTES {
        return Err(StoreError::InvalidConfig(
            "purge collection name is empty or exceeds 16 KiB",
        ));
    }
    Ok(())
}

fn stored_collection(
    collection_id: u64,
    wiki_id: i64,
    name: String,
    generation: i64,
    status: String,
    tombstoned_at: Option<i64>,
    page_count: i64,
) -> Result<StoredCollection, StoreError> {
    let status = stored_collection_status(&status)?;
    let tombstoned_at = tombstoned_at
        .map(|value| sql_u64(value, "invalid collection tombstone time"))
        .transpose()?;
    if (status == CollectionStatus::Active) != tombstoned_at.is_none() {
        return Err(StoreError::CorruptMetadata(
            "collection status and tombstone time disagree",
        ));
    }
    Ok(StoredCollection {
        collection_id: CollectionId::new(collection_id)
            .map_err(|_| StoreError::CorruptMetadata("invalid collection ID"))?,
        wiki_id: sql_id(wiki_id, "invalid wiki ID")?,
        name,
        generation: sql_u64(generation, "invalid collection generation")?,
        status,
        tombstoned_at,
        page_count: sql_u64(page_count, "invalid collection page count")?,
    })
}

fn unix_time() -> Result<i64, StoreError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::ClockBeforeUnixEpoch)?
        .as_secs();
    to_sql_integer(seconds)
}

fn to_sql_integer(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::IntegerOutOfRange(value))
}

fn encode_manifest(manifest: &SyncManifest) -> Result<(ManifestId, Vec<u8>), StoreError> {
    validate_manifest(manifest)?;
    let (media_inventory, media_placements) =
        manifest
            .media_snapshot
            .as_ref()
            .map_or((None, None), |snapshot| {
                (
                    Some(
                        snapshot
                            .inventory
                            .iter()
                            .map(|media| ManifestMediaWire {
                                media_id: media.media_id.get(),
                                source_sha1: media.source_sha1.clone(),
                                content_object_id: media.content_object_id.to_string(),
                                metadata_identity: media.metadata_identity.clone(),
                            })
                            .collect(),
                    ),
                    Some(
                        snapshot
                            .placements
                            .iter()
                            .map(|placement| ManifestMediaPlacementWire {
                                revision_id: placement.revision_id.get(),
                                placement_index: placement.placement_index,
                                media_id: placement.media_id.get(),
                                source_sha1: placement.source_sha1.clone(),
                                content_object_id: placement.content_object_id.to_string(),
                                placement_identity: placement.placement_identity.clone(),
                            })
                            .collect(),
                    ),
                )
            });
    let body = ManifestBody {
        schema_version: MANIFEST_SCHEMA_VERSION,
        sequence: manifest.sequence,
        predecessor: manifest.predecessor.map(|id| id.to_string()),
        run_id: manifest.run_id,
        wiki_id: manifest.wiki_id.get(),
        collection_id: manifest.collection_id.map(CollectionId::get),
        run_kind: manifest.run_kind.as_str().to_owned(),
        source: manifest.source.clone(),
        capture_started_at: manifest.capture_started_at,
        capture_completed_at: manifest.capture_completed_at,
        configuration_hash: manifest.configuration_hash.clone(),
        introduced_revisions: manifest
            .introduced_revisions
            .iter()
            .map(|revision| ManifestRevisionWire {
                page_id: revision.page_id.get(),
                revision_id: revision.revision_id.get(),
                content_object_id: revision.content_object_id.to_string(),
            })
            .collect(),
        page_heads: manifest
            .page_heads
            .iter()
            .map(|head| ManifestPageHeadWire {
                page_id: head.page_id.get(),
                revision_id: head.revision_id.map(RevisionId::get),
            })
            .collect(),
        media_inventory,
        media_placements,
    };
    let canonical_body = serde_json::to_vec(&body)
        .map_err(|_| StoreError::InvalidManifest("manifest body could not be serialized"))?;
    let id = ManifestId::for_body(&canonical_body);
    let envelope = ManifestEnvelope {
        manifest_id: id.to_string(),
        body,
    };
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|_| StoreError::InvalidManifest("manifest could not be serialized"))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(StoreError::ManifestLimitExceeded);
    }
    Ok((id, bytes))
}

fn decode_manifest(sequence: u64, bytes: &[u8]) -> Result<StoredManifest, StoreError> {
    let envelope: ManifestEnvelope =
        serde_json::from_slice(bytes).map_err(|_| StoreError::CorruptManifest {
            sequence,
            message: "manifest is not valid schema-v1 JSON",
        })?;
    if !matches!(envelope.body.schema_version, 1 | MANIFEST_SCHEMA_VERSION) {
        return Err(StoreError::CorruptManifest {
            sequence,
            message: "unsupported manifest schema version",
        });
    }
    if envelope.body.sequence != sequence {
        return Err(StoreError::CorruptManifest {
            sequence,
            message: "filename and manifest sequence disagree",
        });
    }
    let canonical_body =
        serde_json::to_vec(&envelope.body).map_err(|_| StoreError::CorruptManifest {
            sequence,
            message: "manifest body cannot be canonicalized",
        })?;
    let actual_id = ManifestId::for_body(&canonical_body);
    let recorded_id = envelope
        .manifest_id
        .parse()
        .map_err(|_| StoreError::CorruptManifest {
            sequence,
            message: "manifest records an invalid identity",
        })?;
    if actual_id != recorded_id {
        return Err(StoreError::CorruptManifest {
            sequence,
            message: "manifest body does not reproduce its recorded identity",
        });
    }
    let canonical_envelope =
        serde_json::to_vec(&envelope).map_err(|_| StoreError::CorruptManifest {
            sequence,
            message: "manifest cannot be canonicalized",
        })?;
    if canonical_envelope != bytes {
        return Err(StoreError::CorruptManifest {
            sequence,
            message: "manifest file is not in canonical JSON form",
        });
    }
    if envelope.body.introduced_revisions.len() > MAX_MANIFEST_ENTRIES
        || envelope.body.page_heads.len() > MAX_MANIFEST_ENTRIES
        || envelope
            .body
            .media_inventory
            .as_ref()
            .is_some_and(|entries| entries.len() > MAX_MANIFEST_ENTRIES)
        || envelope
            .body
            .media_placements
            .as_ref()
            .is_some_and(|entries| entries.len() > MAX_MANIFEST_ENTRIES)
    {
        return Err(StoreError::CorruptManifest {
            sequence,
            message: "manifest entry count exceeds bounds",
        });
    }
    let predecessor = envelope
        .body
        .predecessor
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|_| StoreError::CorruptManifest {
            sequence,
            message: "manifest predecessor identity is invalid",
        })?;
    let mut introduced_revisions = Vec::with_capacity(envelope.body.introduced_revisions.len());
    for revision in envelope.body.introduced_revisions {
        introduced_revisions.push(ManifestRevision {
            page_id: PageId::new(revision.page_id).map_err(|_| StoreError::CorruptManifest {
                sequence,
                message: "manifest page ID is invalid",
            })?,
            revision_id: RevisionId::new(revision.revision_id).map_err(|_| {
                StoreError::CorruptManifest {
                    sequence,
                    message: "manifest revision ID is invalid",
                }
            })?,
            content_object_id: revision.content_object_id.parse().map_err(|_| {
                StoreError::CorruptManifest {
                    sequence,
                    message: "manifest object ID is invalid",
                }
            })?,
        });
    }
    let mut page_heads = Vec::with_capacity(envelope.body.page_heads.len());
    for head in envelope.body.page_heads {
        page_heads.push(ManifestPageHead {
            page_id: PageId::new(head.page_id).map_err(|_| StoreError::CorruptManifest {
                sequence,
                message: "manifest head page ID is invalid",
            })?,
            revision_id: head
                .revision_id
                .map(RevisionId::new)
                .transpose()
                .map_err(|_| StoreError::CorruptManifest {
                    sequence,
                    message: "manifest head revision ID is invalid",
                })?,
        });
    }
    let media_snapshot = match (
        envelope.body.media_inventory,
        envelope.body.media_placements,
    ) {
        (None, None) if envelope.body.schema_version == 1 => None,
        (Some(inventory), Some(placements)) if envelope.body.schema_version == 2 => {
            let inventory = inventory
                .into_iter()
                .map(|media| {
                    Ok(ManifestMedia {
                        media_id: MediaId::new(media.media_id).map_err(|_| {
                            StoreError::CorruptManifest {
                                sequence,
                                message: "manifest media ID is invalid",
                            }
                        })?,
                        source_sha1: media.source_sha1,
                        content_object_id: media.content_object_id.parse().map_err(|_| {
                            StoreError::CorruptManifest {
                                sequence,
                                message: "manifest media object ID is invalid",
                            }
                        })?,
                        metadata_identity: media.metadata_identity,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            let placements = placements
                .into_iter()
                .map(|placement| {
                    Ok(ManifestMediaPlacement {
                        revision_id: RevisionId::new(placement.revision_id).map_err(|_| {
                            StoreError::CorruptManifest {
                                sequence,
                                message: "manifest media revision ID is invalid",
                            }
                        })?,
                        placement_index: placement.placement_index,
                        media_id: MediaId::new(placement.media_id).map_err(|_| {
                            StoreError::CorruptManifest {
                                sequence,
                                message: "manifest placement media ID is invalid",
                            }
                        })?,
                        source_sha1: placement.source_sha1,
                        content_object_id: placement.content_object_id.parse().map_err(|_| {
                            StoreError::CorruptManifest {
                                sequence,
                                message: "manifest placement object ID is invalid",
                            }
                        })?,
                        placement_identity: placement.placement_identity,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            Some(ManifestMediaSnapshot {
                inventory,
                placements,
            })
        }
        _ => {
            return Err(StoreError::CorruptManifest {
                sequence,
                message: "manifest schema and media coverage fields disagree",
            });
        }
    };
    let manifest = SyncManifest {
        sequence,
        predecessor,
        run_id: envelope.body.run_id,
        wiki_id: WikiId::new(envelope.body.wiki_id).map_err(|_| StoreError::CorruptManifest {
            sequence,
            message: "manifest wiki ID is invalid",
        })?,
        collection_id: envelope
            .body
            .collection_id
            .map(CollectionId::new)
            .transpose()
            .map_err(|_| StoreError::CorruptManifest {
                sequence,
                message: "manifest collection ID is invalid",
            })?,
        run_kind: SyncRunKind::from_database(&envelope.body.run_kind).map_err(|_| {
            StoreError::CorruptManifest {
                sequence,
                message: "manifest run kind is invalid",
            }
        })?,
        source: envelope.body.source,
        capture_started_at: envelope.body.capture_started_at,
        capture_completed_at: envelope.body.capture_completed_at,
        configuration_hash: envelope.body.configuration_hash,
        introduced_revisions,
        page_heads,
        media_snapshot,
    };
    validate_manifest(&manifest).map_err(|_| StoreError::CorruptManifest {
        sequence,
        message: "manifest contents violate schema invariants",
    })?;
    Ok(StoredManifest {
        id: actual_id,
        manifest,
    })
}

fn validate_manifest(manifest: &SyncManifest) -> Result<(), StoreError> {
    if manifest.sequence == 0 || manifest.run_id == 0 {
        return Err(StoreError::InvalidManifest(
            "manifest sequence and run ID must be positive",
        ));
    }
    if manifest.capture_completed_at < manifest.capture_started_at {
        return Err(StoreError::InvalidManifest(
            "manifest capture interval is reversed",
        ));
    }
    validate_manifest_text(&manifest.source)?;
    manifest
        .configuration_hash
        .parse::<ManifestId>()
        .map_err(|_| StoreError::InvalidManifest("configuration hash is invalid"))?;
    if manifest.introduced_revisions.len() > MAX_MANIFEST_ENTRIES
        || manifest.page_heads.len() > MAX_MANIFEST_ENTRIES
        || manifest.media_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.inventory.len() > MAX_MANIFEST_ENTRIES
                || snapshot.placements.len() > MAX_MANIFEST_ENTRIES
        })
    {
        return Err(StoreError::ManifestLimitExceeded);
    }
    if manifest
        .introduced_revisions
        .windows(2)
        .any(|pair| pair[0].revision_id >= pair[1].revision_id)
    {
        return Err(StoreError::InvalidManifest(
            "introduced revisions are not strictly ordered",
        ));
    }
    if manifest
        .page_heads
        .windows(2)
        .any(|pair| pair[0].page_id >= pair[1].page_id)
    {
        return Err(StoreError::InvalidManifest(
            "page heads are not strictly ordered",
        ));
    }
    if let Some(snapshot) = &manifest.media_snapshot {
        if snapshot.inventory.windows(2).any(|pair| {
            (
                &pair[0].media_id,
                &pair[0].source_sha1,
                pair[0].content_object_id,
            ) >= (
                &pair[1].media_id,
                &pair[1].source_sha1,
                pair[1].content_object_id,
            )
        }) {
            return Err(StoreError::InvalidManifest(
                "media inventory is not strictly ordered",
            ));
        }
        if snapshot.placements.windows(2).any(|pair| {
            (pair[0].revision_id, pair[0].placement_index)
                >= (pair[1].revision_id, pair[1].placement_index)
        }) {
            return Err(StoreError::InvalidManifest(
                "media placements are not strictly ordered",
            ));
        }
        for media in &snapshot.inventory {
            validate_manifest_text(&media.source_sha1)?;
            validate_blake3_identity(&media.metadata_identity)?;
        }
        for placement in &snapshot.placements {
            validate_manifest_text(&placement.source_sha1)?;
            validate_blake3_identity(&placement.placement_identity)?;
            if placement.placement_index >= MAX_THUMBNAILS_PER_REVISION {
                return Err(StoreError::InvalidManifest(
                    "manifest media placement index exceeds bounds",
                ));
            }
        }
    }
    Ok(())
}

fn manifest_record_identity<T: Serialize>(domain: &[u8], body: &T) -> Result<String, StoreError> {
    let bytes = serde_json::to_vec(body)
        .map_err(|_| StoreError::InvalidManifest("media identity could not be serialized"))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(&bytes);
    Ok(format!("b3:{}", hasher.finalize().to_hex()))
}

fn validate_blake3_identity(value: &str) -> Result<(), StoreError> {
    value
        .parse::<ManifestId>()
        .map(|_| ())
        .map_err(|_| StoreError::InvalidManifest("media record identity is invalid"))
}

fn validate_manifest_text(value: &str) -> Result<(), StoreError> {
    if value.trim().is_empty()
        || value.len() > MAX_MANIFEST_TEXT_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(StoreError::InvalidManifest(
            "manifest text is empty, too long, or contains controls",
        ));
    }
    Ok(())
}

fn parse_manifest_filename(name: &str) -> Result<u64, StoreError> {
    let expected_length = MANIFEST_FILENAME_DIGITS + ".json".len();
    if name.len() != expected_length || !name.ends_with(".json") {
        return Err(StoreError::InvalidManifest("invalid manifest filename"));
    }
    let digits = &name[..MANIFEST_FILENAME_DIGITS];
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(StoreError::InvalidManifest("invalid manifest filename"));
    }
    let sequence = digits
        .parse::<u64>()
        .map_err(|_| StoreError::InvalidManifest("invalid manifest filename"))?;
    if sequence == 0 {
        return Err(StoreError::InvalidManifest(
            "manifest sequence must be positive",
        ));
    }
    Ok(sequence)
}

fn manifest_configuration_hash_for(
    connection: &Connection,
    wiki_id: WikiId,
    collection_id: Option<CollectionId>,
) -> Result<String, StoreError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"wikisync-manifest-configuration-v4\0");
    hash_manifest_field(&mut hasher, &wiki_id.get().to_string());
    if let Some(collection_id) = collection_id {
        hash_manifest_field(&mut hasher, &collection_id.get().to_string());
        let configuration: ManifestConfigurationRow = connection.query_row(
            "SELECT collections.generation,
                    config.rule_kind, config.category_title, config.category_recursion_depth,
                    history_kind, history_value, maximum_pages, maximum_bytes,
                    removal_policy, image_policy, thumbnail_max_edge_pixels,
                    thumbnail_max_images_per_revision, thumbnail_max_bytes_per_image
             FROM collection_configuration AS config
             JOIN collections USING (collection_id)
             WHERE collection_id = ?1",
            [to_sql_integer(collection_id.get())?],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            },
        )?;
        for field in [
            Some(configuration.0.to_string()),
            Some(configuration.1),
            configuration.2,
            configuration.3.map(|value| value.to_string()),
            Some(configuration.4),
            configuration.5.map(|value| value.to_string()),
            configuration.6.map(|value| value.to_string()),
            configuration.7.map(|value| value.to_string()),
            Some(configuration.8),
            Some(configuration.9),
            configuration.10.map(|value| value.to_string()),
            configuration.11.map(|value| value.to_string()),
            configuration.12.map(|value| value.to_string()),
        ] {
            hash_manifest_optional_field(&mut hasher, field.as_deref());
        }
        let mut statement = connection.prepare(
            "SELECT title FROM collection_rule_titles
             WHERE collection_id = ?1 ORDER BY title",
        )?;
        let titles = statement.query_map([to_sql_integer(collection_id.get())?], |row| {
            row.get::<_, String>(0)
        })?;
        for title in titles {
            hash_manifest_field(&mut hasher, &title?);
        }
    } else {
        hash_manifest_field(&mut hasher, "source-wide");
    }
    let transfer_policy = read_network_transfer_policy(connection)?;
    hash_manifest_field(
        &mut hasher,
        &transfer_policy.max_concurrent_requests().to_string(),
    );
    hash_manifest_optional_field(
        &mut hasher,
        transfer_policy
            .max_download_bytes_per_second()
            .map(|value| value.to_string())
            .as_deref(),
    );
    hash_manifest_field(
        &mut hasher,
        if transfer_policy.avoid_metered_networks() {
            "avoid-metered"
        } else {
            "permit-metered"
        },
    );
    Ok(format!("b3:{}", hasher.finalize().to_hex()))
}

fn hash_manifest_field(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn hash_manifest_optional_field(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hash_manifest_field(hasher, value);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

/// Storage, validation, or integrity failure.
#[derive(Debug)]
pub enum StoreError {
    /// Filesystem or stream failure.
    Io(io::Error),
    /// SQLite failure.
    Sqlite(rusqlite::Error),
    /// A mutating object-store operation was attempted through a read-only library.
    ReadOnly,
    /// A configuration value was invalid.
    InvalidConfig(&'static str),
    /// An object exceeded the configured bound.
    ObjectTooLarge {
        /// Configured maximum.
        limit: u64,
        /// Observed or declared length.
        actual: u64,
    },
    /// A bounded stream or decoded object had an unexpected size.
    LengthMismatch {
        /// Declared metadata length.
        expected: u64,
        /// Number of bytes observed before stopping.
        actual: u64,
    },
    /// No verified physical representation was recorded for the ID.
    ObjectNotFound(ObjectId),
    /// Canonical bytes did not reproduce their logical identity.
    HashMismatch(ObjectId),
    /// A pack or its index violated the immutable on-disk format.
    CorruptPack(&'static str),
    /// A bounded pack size or offset calculation overflowed.
    PackLimitExceeded,
    /// The requested verified pack does not exist.
    PackNotFound(String),
    /// A pack cannot be retired because it contains the only verified copy.
    PackStillRequired(String),
    /// Stored metadata violated a library invariant.
    CorruptMetadata(&'static str),
    /// A collection was used with a different wiki than the one it belongs to.
    CollectionWikiMismatch,
    /// No collection exists for the supplied identity.
    CollectionNotFound(CollectionId),
    /// A mutation or synchronization was requested for a retained collection tombstone.
    CollectionTombstoned(CollectionId),
    /// Destructive payload cleanup was previewed for a collection still being tracked.
    CollectionMustBeTombstoned(CollectionId),
    /// No logical payload is exclusive to the requested tombstoned collection.
    NoExclusivePurgePayload(CollectionId),
    /// A collection-exclusive payload preview exceeded its hard object/work bound.
    PurgeLimitExceeded,
    /// A purge preview exceeded its hard active-location scan bound.
    PurgeLocationLimitExceeded,
    /// A purge preview affected more immutable packs than one operation supports.
    PurgePackLimitExceeded,
    /// A selected logical object has no active verified physical representation.
    PurgeObjectUnavailable(ObjectId),
    /// The operator did not provide both mandatory scope and backup confirmations.
    PurgeAcknowledgementsRequired,
    /// A supplied preview fingerprint was not a canonical BLAKE3 identity.
    InvalidPurgeFingerprint,
    /// The collection tombstone, manifest head, references, or locations changed.
    StalePurgePreview(CollectionId),
    /// A different unfinished purge journal already owns this collection.
    PurgeAlreadyPending(CollectionId),
    /// No durable purge journal exists for the requested identity.
    PurgeNotFound(u64),
    /// An administrative preview was based on an older collection generation.
    StaleCollectionGeneration {
        /// Collection whose preview is stale.
        collection_id: CollectionId,
        /// Generation read before the preview began.
        expected: u64,
        /// Durable generation observed while attempting the commit.
        actual: u64,
    },
    /// A legacy empty collection has not committed a selection rule yet.
    CollectionNotConfigured(CollectionId),
    /// A resolved member reason did not match the committed rule.
    InvalidInclusionReason(PageId),
    /// A capture was attempted for membership removed by reconciliation.
    CollectionMemberNotActive {
        /// Local collection identity.
        collection_id: CollectionId,
        /// Stable remote page identity.
        page_id: PageId,
    },
    /// A hard page-count or canonical-byte collection budget was exceeded.
    CollectionBudgetExceeded {
        /// Budgeted resource (`pages` or `bytes`).
        resource: &'static str,
        /// Configured hard maximum.
        limit: u64,
        /// Estimated use for the requested operation.
        estimated: u64,
    },
    /// A source identity is not registered in this library.
    WikiNotFound(WikiId),
    /// A source registration still owns configuration or retained evidence.
    WikiInUse {
        /// Source that cannot safely be removed.
        wiki_id: WikiId,
        /// Collections still configured for this source.
        collections: u64,
        /// Captured pages whose history depends on this source identity.
        captured_pages: u64,
        /// Synchronization-run evidence for this source.
        sync_runs: u64,
        /// Durable source or collection checkpoints.
        checkpoints: u64,
        /// Immutable manifests naming this source.
        manifests: u64,
    },
    /// Historical content was supplied for a page not yet present in the library.
    PageNotFound {
        /// Source wiki identity.
        wiki_id: WikiId,
        /// Stable remote page identity.
        page_id: PageId,
    },
    /// A page head referred to a revision that is not durable locally.
    RevisionNotFound(RevisionId),
    /// A media placement named a revision owned by a different page.
    RevisionPageMismatch {
        /// Captured revision identity.
        revision_id: RevisionId,
        /// Incorrect requested page identity.
        page_id: PageId,
    },
    /// An existing remote revision was observed with different immutable identity data.
    ConflictingRevision(RevisionId),
    /// Thumbnail metadata or passive-format validation failed.
    InvalidMediaMetadata(&'static str),
    /// An immutable source file version was observed with conflicting metadata.
    ConflictingMedia(MediaId),
    /// One revision placement index was reused for different media or display metadata.
    ConflictingMediaPlacement {
        /// Revision containing the placement.
        revision_id: RevisionId,
        /// Zero-based conflicting placement index.
        placement_index: u32,
    },
    /// A checkpoint boundary preceded the required overlap-window start.
    InvalidCheckpointCandidate {
        /// Existing durable source boundary.
        committed_through: u64,
        /// Candidate supplied by the caller.
        candidate: u64,
    },
    /// A durable job key was reused for different work.
    ConflictingSyncJobKey(String),
    /// A dump identity or authenticated length was malformed or empty.
    InvalidDumpIdentity(&'static str),
    /// Dump import was requested for a non-bootstrap or source-wide run.
    DumpImportRequiresCollectionBootstrap(u64),
    /// The dump race-window timestamp did not match its run checkpoint candidate.
    DumpImportBootstrapStartMismatch {
        /// Owning synchronization run.
        run_id: u64,
        /// Durable checkpoint candidate captured at run start.
        expected: u64,
        /// Timestamp supplied by the import caller.
        actual: u64,
    },
    /// The collection/configuration changed after the owning run began.
    StaleDumpImportConfiguration {
        /// Owning synchronization run.
        run_id: u64,
    },
    /// An existing resume record has a different immutable identity binding.
    DumpImportIdentityMismatch {
        /// Existing local dump-import identity.
        import_id: u64,
    },
    /// A bootstrap run already contains work from a different coordinator.
    DumpImportRunHasExistingJobs {
        /// Incompatible running synchronization run.
        run_id: u64,
        /// Durable jobs present before any dump-import identity existed.
        jobs: u64,
    },
    /// A failed dump import was explicitly classified as non-retryable.
    DumpImportNotRestartable(u64),
    /// A dump import was absent or no longer accepts progress updates.
    DumpImportNotRunning(u64),
    /// A sequential dump cursor attempted to move backwards.
    DumpImportProgressRegression {
        /// Local dump-import identity.
        import_id: u64,
        /// Current durable cursor.
        current: u64,
        /// Regressing cursor requested by the caller.
        requested: u64,
    },
    /// A selected page ledger entry was reused with different revision/byte data.
    ConflictingDumpImportPage {
        /// Stable remote page identity.
        page_id: PageId,
    },
    /// An imported page/revision was not durable and active in the bound selection.
    InvalidDumpImportPage {
        /// Stable remote page identity.
        page_id: PageId,
        /// Current revision read from the dump.
        revision_id: RevisionId,
    },
    /// Durable dump-import counts overflow SQLite's supported range.
    DumpImportProgressOverflow,
    /// A synchronization run was absent or no longer accepts work.
    SyncRunNotRunning(u64),
    /// Another synchronization operation already owns the same checkpoint scope.
    SyncScopeBusy {
        /// Existing local run identity.
        run_id: u64,
        /// Stable kind of the existing operation.
        kind: String,
    },
    /// A synchronization job was absent or not currently claimed.
    SyncJobNotRunning(u64),
    /// A run cannot advance its checkpoint while jobs remain unfinished.
    IncompleteSyncRun {
        /// Local run identity.
        run_id: u64,
        /// Jobs not yet successful.
        incomplete_jobs: u64,
    },
    /// A manifest was requested for an absent, running, or cancelled run.
    SyncRunNotSucceeded(u64),
    /// A legacy run lacks the immutable configuration snapshot needed for a manifest.
    SyncRunConfigurationUnavailable(u64),
    /// A later successful run cannot precede an older unrepresented run in the chain.
    ManifestRunOutOfOrder {
        /// Oldest successful run that must be represented next.
        expected: u64,
        /// Later run requested by the caller.
        requested: u64,
    },
    /// A manifest filename, field, or append request violated a stable invariant.
    InvalidManifest(&'static str),
    /// No manifest exists at the requested sequence.
    ManifestNotFound(u64),
    /// A canonical manifest file failed bounded parsing or identity validation.
    CorruptManifest {
        /// Sequence encoded by the expected filename.
        sequence: u64,
        /// Stable local diagnostic.
        message: &'static str,
    },
    /// A different file already occupies a new manifest sequence.
    ManifestConflict(u64),
    /// Manifest sequence, count, or encoded size exceeded a supported bound.
    ManifestLimitExceeded,
    /// A bounded synchronization key or status string was invalid.
    InvalidSyncText(&'static str),
    /// The database was created by a newer application schema.
    UnsupportedSchemaVersion(u32),
    /// A `u64` value cannot be represented by SQLite's signed integer.
    IntegerOutOfRange(u64),
    /// The system clock cannot provide a conventional capture timestamp.
    ClockBeforeUnixEpoch,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
            Self::ReadOnly => formatter.write_str("library was opened read-only"),
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::ObjectTooLarge { limit, actual } => {
                write!(
                    formatter,
                    "object is {actual} bytes; limit is {limit} bytes"
                )
            }
            Self::LengthMismatch { expected, actual } => {
                write!(
                    formatter,
                    "expected {expected} object bytes, observed {actual}"
                )
            }
            Self::ObjectNotFound(id) => write!(formatter, "object {id} was not found"),
            Self::HashMismatch(id) => write!(formatter, "object {id} failed hash verification"),
            Self::CorruptPack(message) => write!(formatter, "corrupt pack: {message}"),
            Self::PackLimitExceeded => formatter.write_str("pack exceeds configured bounds"),
            Self::PackNotFound(pack_id) => write!(formatter, "pack {pack_id} was not found"),
            Self::PackStillRequired(pack_id) => {
                write!(formatter, "pack {pack_id} still contains a required object")
            }
            Self::CorruptMetadata(message) => write!(formatter, "corrupt metadata: {message}"),
            Self::CollectionWikiMismatch => {
                formatter.write_str("collection does not belong to the requested wiki")
            }
            Self::CollectionNotFound(collection_id) => {
                write!(formatter, "collection {collection_id} was not found")
            }
            Self::CollectionTombstoned(collection_id) => {
                write!(formatter, "collection {collection_id} is no longer tracked")
            }
            Self::CollectionMustBeTombstoned(collection_id) => write!(
                formatter,
                "collection {collection_id} must be tombstoned before payload purge"
            ),
            Self::NoExclusivePurgePayload(collection_id) => write!(
                formatter,
                "collection {collection_id} has no exclusive canonical payload to purge"
            ),
            Self::PurgeLimitExceeded => {
                formatter.write_str("purge preview exceeds supported bounds")
            }
            Self::PurgeLocationLimitExceeded => {
                formatter.write_str("purge preview exceeds the verified-location bound")
            }
            Self::PurgePackLimitExceeded => {
                formatter.write_str("purge preview exceeds the affected-pack bound")
            }
            Self::PurgeObjectUnavailable(object_id) => write!(
                formatter,
                "purge candidate {object_id} has no active verified representation"
            ),
            Self::PurgeAcknowledgementsRequired => formatter
                .write_str("purge requires payload-only and backup/remnant acknowledgements"),
            Self::InvalidPurgeFingerprint => {
                formatter.write_str("purge preview fingerprint is invalid")
            }
            Self::StalePurgePreview(collection_id) => write!(
                formatter,
                "collection {collection_id} purge preview is stale; preview again; no changes were committed"
            ),
            Self::PurgeAlreadyPending(collection_id) => write!(
                formatter,
                "collection {collection_id} already has a different unfinished purge"
            ),
            Self::PurgeNotFound(purge_id) => {
                write!(formatter, "purge journal {purge_id} was not found")
            }
            Self::StaleCollectionGeneration {
                collection_id,
                expected,
                actual,
            } => write!(
                formatter,
                "collection {collection_id} changed while it was being previewed (expected generation {expected}, found {actual}); reload and preview again; no changes were committed"
            ),
            Self::CollectionNotConfigured(collection_id) => {
                write!(
                    formatter,
                    "collection {collection_id} has no committed rule"
                )
            }
            Self::InvalidInclusionReason(page_id) => {
                write!(
                    formatter,
                    "page {page_id} inclusion reason does not match the collection rule"
                )
            }
            Self::CollectionMemberNotActive {
                collection_id,
                page_id,
            } => write!(
                formatter,
                "page {page_id} is no longer active in collection {collection_id}"
            ),
            Self::CollectionBudgetExceeded {
                resource,
                limit,
                estimated,
            } => write!(
                formatter,
                "collection {resource} estimate {estimated} exceeds hard limit {limit}"
            ),
            Self::WikiNotFound(wiki_id) => write!(formatter, "wiki {wiki_id} was not found"),
            Self::WikiInUse {
                wiki_id,
                collections,
                captured_pages,
                sync_runs,
                checkpoints,
                manifests,
            } => write!(
                formatter,
                "wiki {wiki_id} is still in use ({collections} collections, {captured_pages} captured pages, {sync_runs} sync runs, {checkpoints} checkpoints, {manifests} manifests)"
            ),
            Self::PageNotFound { wiki_id, page_id } => {
                write!(
                    formatter,
                    "page {page_id} from wiki {wiki_id} is not captured"
                )
            }
            Self::RevisionNotFound(revision_id) => {
                write!(formatter, "revision {revision_id} is not captured")
            }
            Self::RevisionPageMismatch {
                revision_id,
                page_id,
            } => write!(
                formatter,
                "revision {revision_id} does not belong to page {page_id}"
            ),
            Self::ConflictingRevision(revision_id) => write!(
                formatter,
                "revision {revision_id} conflicts with its previously captured identity"
            ),
            Self::InvalidMediaMetadata(message) => {
                write!(formatter, "invalid thumbnail metadata: {message}")
            }
            Self::ConflictingMedia(media_id) => write!(
                formatter,
                "media {media_id} conflicts with its previously captured source version"
            ),
            Self::ConflictingMediaPlacement {
                revision_id,
                placement_index,
            } => write!(
                formatter,
                "revision {revision_id} media placement {placement_index} conflicts with its previously captured value"
            ),
            Self::InvalidCheckpointCandidate {
                committed_through,
                candidate,
            } => write!(
                formatter,
                "checkpoint candidate {candidate} precedes committed boundary {committed_through}"
            ),
            Self::ConflictingSyncJobKey(key) => {
                write!(
                    formatter,
                    "sync job key {key:?} identifies conflicting work"
                )
            }
            Self::InvalidDumpIdentity(message) => {
                write!(formatter, "invalid authenticated dump identity: {message}")
            }
            Self::DumpImportRequiresCollectionBootstrap(run_id) => write!(
                formatter,
                "sync run {run_id} is not a collection-scoped bootstrap dump import"
            ),
            Self::DumpImportBootstrapStartMismatch {
                run_id,
                expected,
                actual,
            } => write!(
                formatter,
                "dump import for sync run {run_id} started at {actual}, but its durable bootstrap boundary is {expected}"
            ),
            Self::StaleDumpImportConfiguration { run_id } => write!(
                formatter,
                "sync run {run_id} no longer matches the durable collection configuration; the dump import cannot resume"
            ),
            Self::DumpImportIdentityMismatch { import_id } => write!(
                formatter,
                "dump import {import_id} cannot resume with a different dump, selection, configuration, or bootstrap boundary"
            ),
            Self::DumpImportRunHasExistingJobs { run_id, jobs } => write!(
                formatter,
                "sync run {run_id} already contains {jobs} jobs from incompatible bootstrap work and cannot adopt a dump import"
            ),
            Self::DumpImportNotRestartable(import_id) => write!(
                formatter,
                "dump import {import_id} failed permanently and cannot be resumed"
            ),
            Self::DumpImportNotRunning(import_id) => {
                write!(formatter, "dump import {import_id} is not running")
            }
            Self::DumpImportProgressRegression {
                import_id,
                current,
                requested,
            } => write!(
                formatter,
                "dump import {import_id} cursor cannot move backwards from {current} to {requested}"
            ),
            Self::ConflictingDumpImportPage { page_id } => write!(
                formatter,
                "dump import page {page_id} conflicts with its previously recorded revision or byte length"
            ),
            Self::InvalidDumpImportPage {
                page_id,
                revision_id,
            } => write!(
                formatter,
                "dump import page {page_id} revision {revision_id} is not durable and active in the bound collection"
            ),
            Self::DumpImportProgressOverflow => {
                formatter.write_str("dump import progress exceeds the supported SQLite range")
            }
            Self::SyncRunNotRunning(run_id) => {
                write!(formatter, "sync run {run_id} is not running")
            }
            Self::SyncScopeBusy { run_id, kind } => write!(
                formatter,
                "sync run {run_id} ({kind}) already owns this checkpoint scope"
            ),
            Self::SyncJobNotRunning(job_id) => {
                write!(formatter, "sync job {job_id} is not running")
            }
            Self::IncompleteSyncRun {
                run_id,
                incomplete_jobs,
            } => write!(
                formatter,
                "sync run {run_id} has {incomplete_jobs} incomplete jobs"
            ),
            Self::SyncRunNotSucceeded(run_id) => {
                write!(formatter, "sync run {run_id} has not succeeded")
            }
            Self::SyncRunConfigurationUnavailable(run_id) => write!(
                formatter,
                "sync run {run_id} predates durable manifest configuration snapshots"
            ),
            Self::ManifestRunOutOfOrder {
                expected,
                requested,
            } => write!(
                formatter,
                "sync run {requested} cannot be manifested before older successful run {expected}"
            ),
            Self::InvalidManifest(message) => write!(formatter, "invalid manifest: {message}"),
            Self::ManifestNotFound(sequence) => {
                write!(formatter, "manifest sequence {sequence} was not found")
            }
            Self::CorruptManifest { sequence, message } => {
                write!(formatter, "corrupt manifest {sequence}: {message}")
            }
            Self::ManifestConflict(sequence) => {
                write!(formatter, "manifest sequence {sequence} already exists")
            }
            Self::ManifestLimitExceeded => formatter.write_str("manifest exceeds supported bounds"),
            Self::InvalidSyncText(label) => write!(
                formatter,
                "{label} must be non-empty, bounded, and contain no control characters"
            ),
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "database schema version {version} is newer than supported"
                )
            }
            Self::IntegerOutOfRange(value) => {
                write!(formatter, "value {value} exceeds SQLite integer range")
            }
            Self::ClockBeforeUnixEpoch => formatter.write_str("system clock is before Unix epoch"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    const SECOND_VALID_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0xf4,
        0x22, 0x7f, 0x8a, 0x00, 0x00, 0x00, 0x0e, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x04, 0x01, 0x10, 0xf8, 0x03, 0xfd, 0x4e, 0x95, 0xc1, 0x6f, 0x00,
        0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn test_library() -> (tempfile::TempDir, Library) {
        let directory = tempfile::tempdir().expect("temporary library");
        let library = Library::open(directory.path()).expect("open library");
        (directory, library)
    }

    fn capture_test_page(
        library: &mut Library,
        wiki_id: WikiId,
        collection_id: CollectionId,
        page_id: u64,
        revision_id: u64,
        timestamp: &str,
        title: &str,
    ) {
        let title = PageTitle::new(title).expect("fixture title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: PageId::new(page_id).expect("fixture page ID"),
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(revision_id).expect("fixture revision ID"),
                    parent_id: None,
                    timestamp,
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: title.as_str().as_bytes(),
                },
            )
            .expect("capture fixture page");
    }

    fn capture_test_page_source(
        library: &mut Library,
        wiki_id: WikiId,
        collection_id: CollectionId,
        page_id: u64,
        revision_id: u64,
        title: &str,
        source: &[u8],
    ) -> ObjectId {
        let title = PageTitle::new(title).expect("fixture title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: PageId::new(page_id).expect("fixture page ID"),
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(revision_id).expect("fixture revision ID"),
                    parent_id: None,
                    timestamp: "2026-08-24T10:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source,
                },
            )
            .expect("capture fixture page")
            .id
    }

    fn integrity_media_fixture() -> (tempfile::TempDir, Library, ObjectId) {
        let (directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Integrity media fixture")
            .expect("create collection");
        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            41,
            410,
            "2026-08-22T10:00:00Z",
            "Integrity media article",
        );
        let file_title = PageTitle::new("File:Integrity.png").expect("file title");
        let capture = ThumbnailCapture {
            media_id: MediaId::new(9001).expect("media ID"),
            file_title: &file_title,
            source_sha1: "abcdef0123456789abcdef0123456789",
            original_url: "https://upload.wikimedia.org/integrity.png",
            description_url: "https://commons.wikimedia.org/wiki/File:Integrity.png",
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
        let object = library
            .capture_revision_thumbnail(
                wiki_id,
                PageId::new(41).expect("page ID"),
                RevisionId::new(410).expect("revision ID"),
                ThumbnailPolicy::new(640, 8, 1024).expect("thumbnail policy"),
                &capture,
                RevisionMediaPlacement {
                    index: 0,
                    kind: MediaPlacementKind::Lead,
                    caption: Some("Fixture caption"),
                    alt_text: Some("Fixture alternative"),
                },
            )
            .expect("capture thumbnail");
        (directory, library, object.id)
    }

    fn filesystem_snapshot(root: &Path) -> Vec<(PathBuf, bool, u32, [u8; 32])> {
        fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, bool, u32, [u8; 32])>) {
            let metadata = fs::symlink_metadata(path).expect("snapshot metadata");
            #[cfg(unix)]
            let mode = metadata.permissions().mode() & 0o777;
            #[cfg(not(unix))]
            let mode = u32::from(metadata.permissions().readonly());
            let relative = path
                .strip_prefix(root)
                .expect("snapshot path below root")
                .to_path_buf();
            let is_directory = metadata.is_dir();
            let checksum = if metadata.is_file() {
                *blake3::hash(&fs::read(path).expect("snapshot file")).as_bytes()
            } else {
                [0; 32]
            };
            entries.push((relative, is_directory, mode, checksum));
            if is_directory {
                let mut children = fs::read_dir(path)
                    .expect("snapshot directory")
                    .map(|entry| entry.expect("snapshot entry").path())
                    .collect::<Vec<_>>();
                children.sort();
                for child in children {
                    visit(root, &child, entries);
                }
            }
        }

        let mut entries = Vec::new();
        visit(root, root, &mut entries);
        entries
    }

    #[test]
    fn read_only_open_requires_an_existing_database_without_creating_it() {
        let directory = tempfile::tempdir().expect("temporary parent");
        let root = directory.path().join("missing-library");
        fs::create_dir(&root).expect("empty library root");
        let before = filesystem_snapshot(&root);

        let error = Library::open_read_only(&root).expect_err("missing database must fail");
        assert!(matches!(error, StoreError::Io(error) if error.kind() == io::ErrorKind::NotFound));
        assert_eq!(filesystem_snapshot(&root), before);
        assert!(!root.join(DATABASE_NAME).exists());
    }

    #[test]
    fn read_only_open_does_not_migrate_an_old_schema_or_create_layout() {
        let directory = tempfile::tempdir().expect("temporary library");
        let database = directory.path().join(DATABASE_NAME);
        let connection = Connection::open(&database).expect("open version one database");
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                    version INTEGER PRIMARY KEY NOT NULL CHECK (version > 0),
                    name TEXT NOT NULL,
                    applied_at INTEGER NOT NULL
                 ) STRICT;",
            )
            .expect("migration table");
        connection
            .execute_batch(MIGRATION_1)
            .expect("migration one");
        connection
            .execute(
                "INSERT INTO schema_migrations (version, name, applied_at)
                 VALUES (1, 'initial', 0)",
                [],
            )
            .expect("migration record");
        connection
            .pragma_update(None, "user_version", 1)
            .expect("schema version");
        drop(connection);
        let before = filesystem_snapshot(directory.path());

        let library = Library::open_read_only(directory.path()).expect("read old schema");
        assert_eq!(library.schema_version().expect("schema version"), 1);
        assert_eq!(migration_count(&library), 1);
        let query_only: bool = library
            .connection()
            .query_row("PRAGMA query_only", [], |row| row.get(0))
            .expect("query-only state");
        assert!(query_only);
        drop(library);

        assert_eq!(filesystem_snapshot(directory.path()), before);
        let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("inspect old database");
        let revision_table_exists: bool = connection
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name = 'revisions')",
                [],
                |row| row.get(0),
            )
            .expect("inspect old schema");
        assert!(!revision_table_exists);
    }

    #[test]
    fn read_only_open_preserves_initialized_files_while_reads_work_and_writes_fail() {
        let (directory, mut writer) = test_library();
        let wiki_id = writer
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let object = writer
            .put_bytes(ObjectKind::Wikitext, b"read-only canonical bytes")
            .expect("store object");
        drop(writer);

        #[cfg(unix)]
        {
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o750))
                .expect("set distinctive root permissions");
            fs::set_permissions(
                directory.path().join(DATABASE_NAME),
                fs::Permissions::from_mode(0o640),
            )
            .expect("set distinctive database permissions");
        }
        let before = filesystem_snapshot(directory.path());

        let mut library = Library::open_read_only(directory.path()).expect("read-only library");
        assert_eq!(library.schema_version().expect("schema version"), 14);
        assert_eq!(library.wikis().expect("wikis")[0].wiki_id, wiki_id);
        assert_eq!(library.logical_object_count().expect("object count"), 1);
        assert_eq!(
            library
                .logical_objects_after(None, 1)
                .expect("bounded objects")[0]
                .object,
            object
        );
        assert!(library.contains(object.id).expect("contains object"));
        assert_eq!(
            library.read_object(object.id).expect("read object"),
            b"read-only canonical bytes"
        );

        assert!(matches!(
            library.register_wiki("https://example.org/w/api.php", "example"),
            Err(StoreError::Sqlite(_))
        ));
        assert!(matches!(
            library.put_bytes(ObjectKind::Wikitext, b"must not be installed"),
            Err(StoreError::ReadOnly)
        ));
        drop(library);

        assert_eq!(filesystem_snapshot(directory.path()), before);
        let library = Library::open_read_only(directory.path()).expect("reopen read-only library");
        assert_eq!(library.wikis().expect("unchanged wikis").len(), 1);
        assert_eq!(
            library.logical_object_count().expect("unchanged objects"),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn opening_a_library_enforces_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("temporary parent");
        let root = directory.path().join("library");
        fs::create_dir(&root).expect("library root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))
            .expect("insecure root permissions");
        let database = root.join(DATABASE_NAME);
        fs::write(&database, []).expect("empty database");
        fs::set_permissions(&database, fs::Permissions::from_mode(0o644))
            .expect("insecure database permissions");

        let mut library = Library::open(&root).expect("open and harden library");
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for relative in [
            "objects",
            "objects/loose",
            "objects/loose/b3",
            "objects/packs",
            "manifests",
            "tmp",
        ] {
            assert_eq!(
                fs::metadata(root.join(relative))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "{relative} must be private"
            );
        }

        let object = library
            .put_bytes(ObjectKind::Wikitext, b"private object")
            .expect("store object");
        let relative_path: String = library
            .connection()
            .query_row(
                "SELECT relative_path FROM object_locations WHERE object_id = ?1",
                [object.id.to_string()],
                |row| row.get(0),
            )
            .expect("object path");
        assert_eq!(
            fs::metadata(root.join(relative_path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn migration_is_applied_once_and_keeps_locations_separate() {
        let (directory, library) = test_library();
        assert_eq!(library.schema_version().expect("schema version"), 14);
        assert_eq!(migration_count(&library), 14);

        drop(library);
        let reopened = Library::open(directory.path()).expect("reopen library");
        assert_eq!(reopened.schema_version().expect("schema version"), 14);
        assert_eq!(migration_count(&reopened), 14);

        let logical_columns: Vec<String> = reopened
            .connection()
            .prepare("PRAGMA table_info(content_objects)")
            .expect("prepare table info")
            .query_map([], |row| row.get(1))
            .expect("query columns")
            .collect::<Result<_, _>>()
            .expect("collect columns");
        assert!(
            !logical_columns
                .iter()
                .any(|column| column == "relative_path")
        );
    }

    #[test]
    fn version_thirteen_library_adds_empty_purge_journal_without_data_loss() {
        let (directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Migration fixture")
            .expect("collection");
        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            1,
            10,
            "2026-08-24T10:00:00Z",
            "Migration fixture page",
        );
        drop(library);

        let connection = Connection::open(directory.path().join(DATABASE_NAME)).expect("database");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        connection
            .execute_batch(
                "DROP INDEX page_media_by_content_object;
                 DROP INDEX collection_resolved_members_by_page;
                 DROP TABLE purge_pack_work;
                 DROP TABLE purge_objects;
                 DROP INDEX one_unfinished_purge_per_collection;
                 DROP TABLE purge_operations;
                 DELETE FROM schema_migrations WHERE version = 14;
                 PRAGMA user_version = 13;",
            )
            .expect("downgrade purge fixture");
        drop(connection);

        let upgraded = Library::open(directory.path()).expect("upgrade v13 library");
        assert_eq!(upgraded.schema_version().expect("schema version"), 14);
        assert_eq!(migration_count(&upgraded), 14);
        assert_eq!(table_count(&upgraded, "purge_operations"), 0);
        assert_eq!(table_count(&upgraded, "purge_objects"), 0);
        assert_eq!(table_count(&upgraded, "purge_pack_work"), 0);
        assert_eq!(table_count(&upgraded, "revisions"), 1);
        assert_eq!(upgraded.logical_object_count().expect("objects"), 1);
    }

    #[test]
    fn network_transfer_policy_is_bounded_and_defaults_are_explicit() {
        assert_eq!(
            NetworkTransferPolicy::default().max_concurrent_requests(),
            4
        );
        assert_eq!(
            NetworkTransferPolicy::default().max_download_bytes_per_second(),
            None
        );
        assert!(!NetworkTransferPolicy::default().avoid_metered_networks());

        assert!(NetworkTransferPolicy::new(0, None, false).is_err());
        assert!(NetworkTransferPolicy::new(MAX_CONCURRENT_REQUESTS, None, false).is_ok());
        assert!(NetworkTransferPolicy::new(MAX_CONCURRENT_REQUESTS + 1, None, false).is_err());
        assert!(NetworkTransferPolicy::new(1, Some(0), false).is_err());
        assert!(NetworkTransferPolicy::new(1, Some(MAX_DOWNLOAD_BYTES_PER_SECOND), false).is_ok());
        assert!(
            NetworkTransferPolicy::new(1, Some(MAX_DOWNLOAD_BYTES_PER_SECOND + 1), false).is_err()
        );
    }

    #[test]
    fn network_transfer_policy_update_is_atomic_durable_and_read_only_safe() {
        let (directory, mut library) = test_library();
        assert_eq!(
            library
                .network_transfer_policy()
                .expect("default transfer policy"),
            NetworkTransferPolicy::default()
        );
        let configured =
            NetworkTransferPolicy::new(8, Some(1_000_000), true).expect("valid transfer policy");
        library
            .update_network_transfer_policy(configured)
            .expect("persist transfer policy");
        assert_eq!(
            library.network_transfer_policy().expect("updated policy"),
            configured
        );
        drop(library);

        let mut read_only = Library::open_read_only(directory.path()).expect("read-only library");
        assert_eq!(
            read_only.network_transfer_policy().expect("durable policy"),
            configured
        );
        assert!(matches!(
            read_only.update_network_transfer_policy(NetworkTransferPolicy::default()),
            Err(StoreError::ReadOnly)
        ));
        drop(read_only);

        let reopened = Library::open(directory.path()).expect("reopen library");
        assert_eq!(
            reopened
                .network_transfer_policy()
                .expect("unchanged policy"),
            configured
        );
    }

    #[test]
    fn new_sync_runs_snapshot_the_network_transfer_policy() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let first = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start first run");
        let first_hash = first
            .status
            .configuration_hash
            .clone()
            .expect("first configuration hash");

        library
            .update_network_transfer_policy(
                NetworkTransferPolicy::new(2, Some(500_000), true).expect("policy"),
            )
            .expect("update transfer policy");
        let resumed = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("resume first run");
        assert!(resumed.resumed);
        assert_eq!(
            resumed.status.configuration_hash.as_deref(),
            Some(first_hash.as_str())
        );

        library
            .cancel_sync_run(first.status.run_id)
            .expect("cancel first run");
        let second = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start second run");
        assert_ne!(second.status.configuration_hash, Some(first_hash));
    }

    #[test]
    fn network_transfer_policy_reads_fail_closed_on_corrupt_values() {
        let (_directory, library) = test_library();
        library
            .connection()
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("permit corruption fixture");

        library
            .connection()
            .execute(
                "UPDATE network_transfer_policy SET max_concurrent_requests = 0",
                [],
            )
            .expect("corrupt concurrency");
        assert!(matches!(
            library.network_transfer_policy(),
            Err(StoreError::CorruptMetadata(_))
        ));

        library
            .connection()
            .execute(
                "UPDATE network_transfer_policy
                 SET max_concurrent_requests = 4, max_download_bytes_per_second = 0",
                [],
            )
            .expect("corrupt rate");
        assert!(matches!(
            library.network_transfer_policy(),
            Err(StoreError::CorruptMetadata(_))
        ));

        library
            .connection()
            .execute(
                "UPDATE network_transfer_policy
                 SET max_download_bytes_per_second = NULL, avoid_metered_networks = 2",
                [],
            )
            .expect("corrupt metered flag");
        assert!(matches!(
            library.network_transfer_policy(),
            Err(StoreError::CorruptMetadata(_))
        ));
    }

    #[test]
    fn version_eight_library_upgrades_with_default_network_transfer_policy() {
        let directory = tempfile::tempdir().expect("temporary library");
        let library = Library::open(directory.path()).expect("create current library");
        drop(library);
        let connection = Connection::open(directory.path().join(DATABASE_NAME))
            .expect("open database for version-eight fixture");
        connection
            .execute_batch(
                "DROP INDEX page_media_by_content_object;
                 DROP INDEX collection_resolved_members_by_page;
                 DROP TABLE purge_pack_work;
                 DROP TABLE purge_objects;
                 DROP TABLE purge_operations;
                 DROP TABLE dump_import_pages;
                 DROP TABLE dump_imports;
                 DROP TABLE page_media;
                 DROP TABLE media;
                 DROP TRIGGER collection_configuration_image_policy_insert;
                 DROP TRIGGER collection_configuration_image_policy_update;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_bytes_per_image;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_images_per_revision;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_edge_pixels;
                 ALTER TABLE collection_configuration DROP COLUMN image_policy;
                 DROP INDEX revisions_by_content_affinity;
                 DROP TABLE network_transfer_policy;
                 DROP INDEX collections_by_status_name;
                 ALTER TABLE collections DROP COLUMN status;
                 ALTER TABLE collections DROP COLUMN generation;
                 ALTER TABLE collections DROP COLUMN tombstoned_at;
                 DELETE FROM schema_migrations WHERE version IN (9, 10, 11, 12, 13, 14);
                 PRAGMA user_version = 8;",
            )
            .expect("downgrade fixture metadata");
        drop(connection);

        let upgraded = Library::open(directory.path()).expect("upgrade version eight library");
        assert_eq!(upgraded.schema_version().expect("schema version"), 14);
        assert_eq!(migration_count(&upgraded), 14);
        assert_eq!(
            upgraded
                .network_transfer_policy()
                .expect("migrated default policy"),
            NetworkTransferPolicy::default()
        );
    }

    #[test]
    fn version_nine_collection_fixture_upgrades_active_without_losing_run_scope() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("create current library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Legacy collection")
            .expect("legacy collection");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("legacy run")
            .status
            .run_id;
        library.cancel_sync_run(run_id).expect("finish fixture run");
        drop(library);

        let connection = Connection::open(directory.path().join(DATABASE_NAME))
            .expect("open database for version-nine fixture");
        connection
            .execute_batch(
                "DROP INDEX page_media_by_content_object;
                 DROP INDEX collection_resolved_members_by_page;
                 DROP TABLE purge_pack_work;
                 DROP TABLE purge_objects;
                 DROP TABLE purge_operations;
                 DROP TABLE dump_import_pages;
                 DROP TABLE dump_imports;
                 DROP TABLE page_media;
                 DROP TABLE media;
                 DROP TRIGGER collection_configuration_image_policy_insert;
                 DROP TRIGGER collection_configuration_image_policy_update;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_bytes_per_image;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_images_per_revision;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_edge_pixels;
                 ALTER TABLE collection_configuration DROP COLUMN image_policy;
                 DROP INDEX revisions_by_content_affinity;
                 DROP INDEX collections_by_status_name;
                 ALTER TABLE collections DROP COLUMN status;
                 ALTER TABLE collections DROP COLUMN generation;
                 ALTER TABLE collections DROP COLUMN tombstoned_at;
                 DELETE FROM schema_migrations WHERE version IN (10, 11, 12, 13, 14);
                 PRAGMA user_version = 9;",
            )
            .expect("downgrade fixture metadata");
        drop(connection);

        let upgraded = Library::open(directory.path()).expect("upgrade version nine fixture");
        assert_eq!(upgraded.schema_version().expect("schema version"), 14);
        assert_eq!(migration_count(&upgraded), 14);
        let collection = upgraded
            .collection(collection_id)
            .expect("collection lookup")
            .expect("migrated collection");
        assert_eq!(collection.status, CollectionStatus::Active);
        assert_eq!(collection.tombstoned_at, None);
        assert_eq!(collection.generation, 1);
        assert_eq!(
            upgraded
                .sync_run_status(run_id)
                .expect("run lookup")
                .expect("retained run")
                .collection_id,
            Some(collection_id)
        );
    }

    #[test]
    fn version_eleven_library_adds_default_off_media_schema_without_data_loss() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("create current library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([PageTitle::new("Migration fixture").expect("fixture title")])
                .expect("fixture selection"),
        );
        let collection_id = library
            .create_collection(
                wiki_id,
                "Version eleven fixture",
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited(),
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect("create fixture collection");
        let object = library
            .put_bytes(ObjectKind::Wikitext, b"pre-media canonical object")
            .expect("store pre-media object");
        drop(library);

        let connection = Connection::open(directory.path().join(DATABASE_NAME))
            .expect("open database for version-eleven fixture");
        connection
            .execute_batch(
                "DROP INDEX page_media_by_content_object;
                 DROP INDEX collection_resolved_members_by_page;
                 DROP TABLE purge_pack_work;
                 DROP TABLE purge_objects;
                 DROP TABLE purge_operations;
                 DROP TABLE dump_import_pages;
                 DROP TABLE dump_imports;
                 DROP TABLE page_media;
                 DROP TABLE media;
                 DROP TRIGGER collection_configuration_image_policy_insert;
                 DROP TRIGGER collection_configuration_image_policy_update;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_bytes_per_image;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_images_per_revision;
                 ALTER TABLE collection_configuration DROP COLUMN thumbnail_max_edge_pixels;
                 ALTER TABLE collection_configuration DROP COLUMN image_policy;
                 DELETE FROM schema_migrations WHERE version IN (12, 13, 14);
                 PRAGMA user_version = 11;",
            )
            .expect("downgrade fixture metadata");
        drop(connection);

        let upgraded = Library::open(directory.path()).expect("upgrade version eleven library");
        assert_eq!(upgraded.schema_version().expect("schema version"), 14);
        assert_eq!(migration_count(&upgraded), 14);
        assert_eq!(
            upgraded
                .collection_configuration(collection_id)
                .expect("read upgraded collection")
                .expect("configured collection")
                .image_policy,
            ImagePolicy::None
        );
        assert_eq!(
            upgraded.read_object(object.id).expect("retained object"),
            b"pre-media canonical object"
        );
        assert_eq!(table_count(&upgraded, "media"), 0);
        assert_eq!(table_count(&upgraded, "page_media"), 0);
    }

    #[test]
    fn object_identity_is_versioned_and_kind_separated() {
        let source = b"== Rust ==\nA language.";
        let wikitext = ObjectId::for_bytes(ObjectKind::Wikitext, source);
        let media = ObjectId::for_bytes(ObjectKind::Media, source);

        assert_ne!(wikitext, media);
        assert_eq!(
            wikitext.to_string().parse::<ObjectId>().expect("parse ID"),
            wikitext
        );
    }

    #[test]
    fn version_one_library_upgrades_to_current_schema() {
        let directory = tempfile::tempdir().expect("temporary library");
        let database = directory.path().join(DATABASE_NAME);
        let connection = Connection::open(database).expect("open version one database");
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                    version INTEGER PRIMARY KEY NOT NULL CHECK (version > 0),
                    name TEXT NOT NULL,
                    applied_at INTEGER NOT NULL
                 ) STRICT;",
            )
            .expect("migration table");
        connection
            .execute_batch(MIGRATION_1)
            .expect("migration one");
        connection
            .execute(
                "INSERT INTO schema_migrations (version, name, applied_at)
                 VALUES (1, 'initial', 0)",
                [],
            )
            .expect("migration record");
        connection
            .pragma_update(None, "user_version", 1)
            .expect("schema version");
        drop(connection);

        let upgraded = Library::open(directory.path()).expect("upgrade library");
        assert_eq!(upgraded.schema_version().expect("schema version"), 14);
        assert_eq!(migration_count(&upgraded), 14);
        assert_eq!(table_count(&upgraded, "revisions"), 0);
    }

    #[test]
    fn version_two_library_upgrades_to_contentless_search_schema() {
        let directory = tempfile::tempdir().expect("temporary library");
        let database = directory.path().join(DATABASE_NAME);
        let connection = Connection::open(database).expect("open version two database");
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                    version INTEGER PRIMARY KEY NOT NULL CHECK (version > 0),
                    name TEXT NOT NULL,
                    applied_at INTEGER NOT NULL
                 ) STRICT;",
            )
            .expect("migration table");
        connection
            .execute_batch(MIGRATION_1)
            .expect("migration one");
        connection
            .execute_batch(MIGRATION_2)
            .expect("migration two");
        connection
            .execute_batch(
                "INSERT INTO schema_migrations (version, name, applied_at)
                 VALUES (1, 'initial', 0), (2, 'capture', 0);",
            )
            .expect("migration records");
        connection
            .pragma_update(None, "user_version", 2)
            .expect("schema version");
        drop(connection);

        let upgraded = Library::open(directory.path()).expect("upgrade library");
        assert_eq!(upgraded.schema_version().expect("schema version"), 14);
        assert_eq!(migration_count(&upgraded), 14);
        assert_eq!(table_count(&upgraded, "search_documents"), 0);
        let fts_definition: String = upgraded
            .connection()
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name = 'search_fts'",
                [],
                |row| row.get(0),
            )
            .expect("FTS schema");
        assert!(fts_definition.contains("content=''"));
        assert!(fts_definition.contains("contentless_delete=1"));
    }

    #[test]
    fn schedule_types_and_configuration_are_bounded() {
        assert!(ScheduleCadence::interval(MIN_SCHEDULE_INTERVAL_SECONDS).is_ok());
        assert!(ScheduleCadence::interval(MAX_SCHEDULE_INTERVAL_SECONDS).is_ok());
        assert!(ScheduleCadence::interval(MIN_SCHEDULE_INTERVAL_SECONDS - 1).is_err());
        assert!(ScheduleCadence::daily_utc(86_399).is_ok());
        assert!(ScheduleCadence::daily_utc(86_400).is_err());

        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Scheduled")
            .expect("create collection");
        assert!(matches!(
            library
                .set_collection_schedule(collection_id, ScheduleCadence::Manual, 1, false, None,),
            Err(StoreError::InvalidConfig(_))
        ));
        assert!(matches!(
            library.set_collection_schedule(
                collection_id,
                ScheduleCadence::interval(60).expect("interval"),
                61,
                false,
                Some(100),
            ),
            Err(StoreError::InvalidConfig(_))
        ));
        assert!(matches!(
            library.set_collection_schedule(
                collection_id,
                ScheduleCadence::daily_utc(0).expect("daily"),
                0,
                false,
                None,
            ),
            Err(StoreError::InvalidConfig(_))
        ));
        assert!(matches!(
            library.due_schedules(100, 0),
            Err(StoreError::InvalidConfig(_))
        ));
    }

    #[test]
    fn schedules_persist_and_due_listing_is_ordered_and_bounded() {
        let (directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let first = library
            .create_explicit_collection(wiki_id, "First")
            .expect("first collection");
        let second = library
            .create_explicit_collection(wiki_id, "Second")
            .expect("second collection");
        let manual = library
            .create_explicit_collection(wiki_id, "Manual")
            .expect("manual collection");
        library
            .set_collection_schedule(
                first,
                ScheduleCadence::interval(300).expect("interval"),
                30,
                false,
                Some(200),
            )
            .expect("first schedule");
        library
            .set_collection_schedule(
                second,
                ScheduleCadence::daily_utc(7 * 60 * 60).expect("daily"),
                600,
                false,
                Some(100),
            )
            .expect("second schedule");
        library
            .set_collection_schedule(manual, ScheduleCadence::Manual, 0, false, None)
            .expect("manual schedule");

        assert_eq!(
            library
                .due_schedules(200, 1)
                .expect("bounded due schedules")
                .iter()
                .map(|schedule| schedule.collection_id)
                .collect::<Vec<_>>(),
            [second]
        );
        assert_eq!(
            library
                .due_schedules(200, 10)
                .expect("due schedules")
                .iter()
                .map(|schedule| schedule.collection_id)
                .collect::<Vec<_>>(),
            [second, first]
        );
        drop(library);

        let reopened = Library::open(directory.path()).expect("reopen library");
        assert_eq!(reopened.schedules().expect("list schedules").len(), 3);
        assert_eq!(
            reopened
                .collection_schedule(first)
                .expect("read schedule")
                .expect("first schedule"),
            CollectionSchedule {
                collection_id: first,
                cadence: ScheduleCadence::interval(300).expect("interval"),
                jitter_seconds: 30,
                paused: false,
                next_run_at: Some(200),
                last_started_at: None,
            }
        );
    }

    #[test]
    fn due_schedule_claim_is_atomic_and_survives_restart_and_sleep() {
        let (directory, mut first_writer) = test_library();
        let wiki_id = first_writer
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = first_writer
            .create_explicit_collection(wiki_id, "Atomic")
            .expect("create collection");
        first_writer
            .set_collection_schedule(
                collection_id,
                ScheduleCadence::interval(300).expect("interval"),
                10,
                false,
                Some(100),
            )
            .expect("set schedule");
        let mut second_writer = Library::open(directory.path()).expect("second writer");
        assert_eq!(
            second_writer
                .due_schedules(10_000, 1)
                .expect("observe due schedule")[0]
                .next_run_at,
            Some(100)
        );

        let claimed = first_writer
            .claim_due_schedule(collection_id, 100, 10_000, 10_300)
            .expect("claim due schedule")
            .expect("won claim");
        assert_eq!(claimed.last_started_at, Some(10_000));
        assert_eq!(claimed.next_run_at, Some(10_300));
        assert!(
            second_writer
                .claim_due_schedule(collection_id, 100, 10_000, 10_300)
                .expect("lose stale claim")
                .is_none()
        );
        drop(first_writer);
        drop(second_writer);

        let reopened = Library::open(directory.path()).expect("reopen library");
        let recovered = reopened
            .collection_schedule(collection_id)
            .expect("read recovered schedule")
            .expect("persisted schedule");
        assert_eq!(recovered.last_started_at, Some(10_000));
        assert_eq!(recovered.next_run_at, Some(10_300));
        assert!(
            reopened
                .due_schedules(10_299, 1)
                .expect("not due yet")
                .is_empty()
        );
    }

    #[test]
    fn loose_objects_round_trip_and_deduplicate() {
        let (_directory, mut library) = test_library();
        let bytes = "WikiSyncer canonical source".repeat(1_000).into_bytes();

        let first = library
            .put_bytes(ObjectKind::Wikitext, &bytes)
            .expect("store object");
        let second = library
            .put_reader(ObjectKind::Wikitext, bytes.len() as u64, bytes.as_slice())
            .expect("store duplicate");

        assert_eq!(first, second);
        assert!(library.contains(first.id).expect("contains object"));
        assert_eq!(library.read_object(first.id).expect("read object"), bytes);
        assert_eq!(table_count(&library, "content_objects"), 1);
        assert_eq!(table_count(&library, "object_locations"), 1);
    }

    #[test]
    fn collection_configuration_and_preview_membership_round_trip_atomically() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        assert_eq!(
            library.wikis().expect("list wikis"),
            [StoredWiki {
                wiki_id,
                api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
                language_code: "en".to_owned(),
            }]
        );
        let category = PageTitle::new("Category:Physics").expect("category");
        let rule = CollectionRule::Category {
            title: category.clone(),
            recursion_depth: 2,
        };
        let budget = CollectionBudget::unlimited()
            .with_maximum_pages(10)
            .expect("page budget")
            .with_maximum_bytes(1_000)
            .expect("byte budget");
        let collection_id = library
            .create_collection(
                wiki_id,
                "Physics",
                &rule,
                HistoryPolicy::last_n(5).expect("history"),
                budget,
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect("create configured collection");
        let configuration = library
            .collection_configuration(collection_id)
            .expect("configuration query")
            .expect("configured collection");
        assert_eq!(configuration.rule, rule);
        assert_eq!(configuration.budget, budget);

        let first = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page ID"),
            namespace: 0,
            title: PageTitle::new("Physics").expect("title"),
            inclusion_reason: InclusionReason::Category {
                category: category.clone(),
                depth: 0,
            },
        };
        let second = ResolvedCollectionMember {
            page_id: PageId::new(11).expect("page ID"),
            namespace: 0,
            title: PageTitle::new("Mechanics").expect("title"),
            inclusion_reason: InclusionReason::Category {
                category: category.clone(),
                depth: 2,
            },
        };
        assert_eq!(
            library
                .commit_resolved_membership(collection_id, &[first.clone(), second.clone()])
                .expect("commit preview"),
            MembershipCommit {
                active_members: 2,
                removed_members: 0,
            }
        );
        assert_eq!(
            library
                .resolved_collection_members(collection_id)
                .expect("resolved members"),
            [first.clone(), second.clone()]
        );
        assert_eq!(table_count(&library, "collection_pages"), 0);

        let revision_id = RevisionId::new(20).expect("revision ID");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: first.page_id,
                    namespace: first.namespace,
                    title: &first.title,
                    revision_id,
                    parent_id: None,
                    timestamp: "2026-08-21T10:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"physics",
                },
            )
            .expect("capture resolved member");
        assert_eq!(table_count(&library, "collection_pages"), 1);
        assert_eq!(
            library
                .resolved_collection_members(collection_id)
                .expect("reason survives capture")[0]
                .inclusion_reason,
            first.inclusion_reason
        );
        assert_eq!(
            library
                .record_collection_estimate(collection_id, 2, Some(900))
                .expect("record estimate"),
            CollectionEstimate {
                resolved_page_count: 2,
                current_canonical_bytes: 7,
                predicted_canonical_bytes: Some(900),
                predicted_at: library
                    .collection_estimate(collection_id)
                    .expect("estimate")
                    .predicted_at,
            }
        );

        assert_eq!(
            library
                .commit_resolved_membership(collection_id, std::slice::from_ref(&second))
                .expect("replace preview"),
            MembershipCommit {
                active_members: 1,
                removed_members: 1,
            }
        );
        assert_eq!(
            library
                .resolved_collection_members(collection_id)
                .expect("replacement"),
            [second]
        );
        assert!(
            library
                .revision(wiki_id, revision_id)
                .expect("retained history")
                .is_some()
        );
    }

    #[test]
    fn atomic_preview_create_and_edit_roll_back_every_field_on_failure() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let title = PageTitle::new("Atomic page").expect("title");
        let selection = TitleSelection::new([title.clone()]).expect("selection");
        let rule = CollectionRule::ExplicitTitles(selection);
        let member = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page ID"),
            namespace: 0,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title.clone()),
        };
        let missing = PageTitle::new("Missing atomic page").expect("missing title");
        let oversized = CollectionPreviewCommit {
            rule: &rule,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited()
                .with_maximum_pages(1)
                .expect("budget"),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            members: &[member.clone(), member.clone()],
            missing_titles: std::slice::from_ref(&missing),
            predicted_canonical_bytes: Some(100),
        };
        assert!(matches!(
            library.create_collection_from_preview(wiki_id, "Rejected", oversized),
            Err(StoreError::CollectionBudgetExceeded {
                resource: "pages",
                ..
            }) | Err(StoreError::InvalidConfig(_))
        ));
        assert!(library.collections().expect("no orphan drafts").is_empty());

        let accepted = CollectionPreviewCommit {
            members: std::slice::from_ref(&member),
            ..oversized
        };
        let (collection_id, membership) = library
            .create_collection_from_preview(wiki_id, "Accepted", accepted)
            .expect("atomic create");
        assert_eq!(membership.active_members, 1);
        assert_eq!(
            library
                .unresolved_titles(collection_id)
                .expect("missing titles"),
            std::slice::from_ref(&missing)
        );

        let before_configuration = library
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured");
        let before_members = library
            .resolved_collection_members(collection_id)
            .expect("members");
        let rejected_edit = CollectionPreviewCommit {
            budget: CollectionBudget::unlimited()
                .with_maximum_pages(1)
                .expect("budget"),
            members: &[
                member.clone(),
                ResolvedCollectionMember {
                    page_id: PageId::new(11).expect("page ID"),
                    ..member.clone()
                },
            ],
            missing_titles: &[],
            ..accepted
        };
        assert!(matches!(
            library.update_collection_from_preview(
                collection_id,
                before_configuration.generation,
                Some("Partially applied"),
                rejected_edit,
            ),
            Err(StoreError::CollectionBudgetExceeded {
                resource: "pages",
                ..
            })
        ));
        assert_eq!(
            library
                .collection(collection_id)
                .expect("collection")
                .expect("present")
                .name,
            "Accepted"
        );
        assert_eq!(
            library
                .collection_configuration(collection_id)
                .expect("configuration")
                .expect("configured"),
            before_configuration
        );
        assert_eq!(
            library
                .resolved_collection_members(collection_id)
                .expect("members"),
            before_members
        );
    }

    #[test]
    fn image_policy_preview_create_and_edit_are_atomic_and_generation_safe() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let first_title = PageTitle::new("Image policy first").expect("first title");
        let second_title = PageTitle::new("Image policy second").expect("second title");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([first_title.clone(), second_title.clone()]).expect("selection"),
        );
        let first_member = ResolvedCollectionMember {
            page_id: PageId::new(71).expect("first page ID"),
            namespace: 0,
            title: first_title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(first_title),
        };
        let second_member = ResolvedCollectionMember {
            page_id: PageId::new(72).expect("second page ID"),
            namespace: 0,
            title: second_title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(second_title),
        };
        let first_members = [first_member.clone()];
        let second_members = [second_member.clone()];
        let initial = CollectionPreviewCommit {
            rule: &rule,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            members: &first_members,
            missing_titles: &[],
            predicted_canonical_bytes: Some(100),
        };
        let thumbnails = ThumbnailPolicy::new(800, 12, 2 * 1024 * 1024).expect("thumbnail policy");

        library
            .connection()
            .execute_batch(
                "CREATE TRIGGER reject_atomic_thumbnail_create
                 BEFORE INSERT ON collection_configuration
                 WHEN NEW.image_policy = 'thumbnails'
                 BEGIN
                     SELECT RAISE(ABORT, 'fixture image policy failure');
                 END;",
            )
            .expect("install create failure trigger");
        assert!(matches!(
            library.create_collection_from_preview_with_image_policy(
                wiki_id,
                "Rejected image policy",
                initial,
                ImagePolicy::Thumbnails(thumbnails),
            ),
            Err(StoreError::Sqlite(_))
        ));
        library
            .connection()
            .execute_batch("DROP TRIGGER reject_atomic_thumbnail_create;")
            .expect("remove create failure trigger");
        assert!(
            library
                .collections()
                .expect("no partial collection")
                .is_empty()
        );

        let (collection_id, membership) = library
            .create_collection_from_preview_with_image_policy(
                wiki_id,
                "Atomic image policy",
                initial,
                ImagePolicy::Thumbnails(thumbnails),
            )
            .expect("create with image policy");
        assert_eq!(membership.active_members, 1);
        let created = library
            .collection_configuration(collection_id)
            .expect("created configuration")
            .expect("configured collection");
        assert_eq!(created.generation, 1);
        assert_eq!(created.image_policy, ImagePolicy::Thumbnails(thumbnails));

        let legacy_edit = CollectionPreviewCommit {
            members: &second_members,
            predicted_canonical_bytes: Some(200),
            ..initial
        };
        library
            .update_collection_from_preview(
                collection_id,
                created.generation,
                Some("Legacy edit preserves images"),
                legacy_edit,
            )
            .expect("legacy image-preserving edit");
        let after_legacy = library
            .collection_configuration(collection_id)
            .expect("legacy configuration")
            .expect("configured collection");
        assert_eq!(after_legacy.generation, created.generation + 1);
        assert_eq!(
            after_legacy.image_policy,
            ImagePolicy::Thumbnails(thumbnails)
        );

        library
            .connection()
            .execute_batch(
                "CREATE TRIGGER reject_atomic_image_policy_edit
                 BEFORE UPDATE ON collection_configuration
                 WHEN NEW.image_policy = 'none'
                 BEGIN
                     SELECT RAISE(ABORT, 'fixture image policy failure');
                 END;",
            )
            .expect("install edit failure trigger");
        let failed_edit = CollectionPreviewCommit {
            members: &first_members,
            predicted_canonical_bytes: Some(300),
            ..initial
        };
        assert!(matches!(
            library.update_collection_from_preview_with_image_policy(
                collection_id,
                after_legacy.generation,
                Some("Must roll back"),
                failed_edit,
                ImagePolicy::None,
            ),
            Err(StoreError::Sqlite(_))
        ));
        library
            .connection()
            .execute_batch("DROP TRIGGER reject_atomic_image_policy_edit;")
            .expect("remove edit failure trigger");
        assert_eq!(
            library
                .collection_configuration(collection_id)
                .expect("configuration after rollback")
                .expect("configured collection"),
            after_legacy
        );
        assert_eq!(
            library
                .resolved_collection_members(collection_id)
                .expect("members after rollback"),
            second_members
        );
        assert_eq!(
            library
                .collection(collection_id)
                .expect("collection after rollback")
                .expect("present")
                .name,
            "Legacy edit preserves images"
        );

        library
            .update_collection_from_preview_with_image_policy(
                collection_id,
                after_legacy.generation,
                Some("Images disabled atomically"),
                failed_edit,
                ImagePolicy::None,
            )
            .expect("atomic image-policy edit");
        let durable = library
            .collection_configuration(collection_id)
            .expect("durable configuration")
            .expect("configured collection");
        assert_eq!(durable.generation, after_legacy.generation + 1);
        assert_eq!(durable.image_policy, ImagePolicy::None);
        assert_eq!(
            library
                .resolved_collection_members(collection_id)
                .expect("durable members"),
            first_members
        );

        assert!(matches!(
            library.update_collection_from_preview_with_image_policy(
                collection_id,
                after_legacy.generation,
                Some("Stale image policy"),
                legacy_edit,
                ImagePolicy::Thumbnails(thumbnails),
            ),
            Err(StoreError::StaleCollectionGeneration {
                collection_id: stale_id,
                expected,
                actual,
            }) if stale_id == collection_id
                && expected == after_legacy.generation
                && actual == durable.generation
        ));
        assert_eq!(
            library
                .collection_configuration(collection_id)
                .expect("configuration after stale edit")
                .expect("configured collection"),
            durable
        );
    }

    #[test]
    fn stale_preview_generation_rejects_a_racing_writer_and_rolls_back_every_field() {
        let (directory, mut first_writer) = test_library();
        let wiki_id = first_writer
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let first_title = PageTitle::new("First generation page").expect("title");
        let second_title = PageTitle::new("Second generation page").expect("title");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([first_title.clone(), second_title.clone()]).expect("selection"),
        );
        let first_member = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page ID"),
            namespace: 0,
            title: first_title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(first_title),
        };
        let second_member = ResolvedCollectionMember {
            page_id: PageId::new(11).expect("page ID"),
            namespace: 0,
            title: second_title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(second_title),
        };
        let initial = CollectionPreviewCommit {
            rule: &rule,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            members: std::slice::from_ref(&first_member),
            missing_titles: &[],
            predicted_canonical_bytes: Some(10),
        };
        let (collection_id, _) = first_writer
            .create_collection_from_preview(wiki_id, "Original", initial)
            .expect("create collection");
        let stale_generation = first_writer
            .collection(collection_id)
            .expect("collection")
            .expect("present")
            .generation;
        assert_eq!(stale_generation, 1);
        let mut second_writer = Library::open(directory.path()).expect("second writer");
        assert_eq!(
            second_writer
                .collection(collection_id)
                .expect("collection")
                .expect("present")
                .generation,
            stale_generation
        );

        let first_commit = CollectionPreviewCommit {
            members: std::slice::from_ref(&second_member),
            predicted_canonical_bytes: Some(20),
            ..initial
        };
        first_writer
            .update_collection_from_preview(
                collection_id,
                stale_generation,
                Some("First writer won"),
                first_commit,
            )
            .expect("first writer commit");
        let durable_after_first = first_writer
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured");
        assert_eq!(durable_after_first.generation, stale_generation + 1);

        let stale_error = second_writer
            .update_collection_from_preview(
                collection_id,
                stale_generation,
                Some("Stale writer must roll back"),
                initial,
            )
            .expect_err("stale preview");
        assert!(matches!(
            stale_error,
            StoreError::StaleCollectionGeneration {
                collection_id: stale_id,
                expected,
                actual,
            } if stale_id == collection_id
                && expected == stale_generation
                && actual == stale_generation + 1
        ));
        let after_stale = second_writer
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured");
        assert_eq!(after_stale, durable_after_first);
        assert_eq!(
            second_writer
                .resolved_collection_members(collection_id)
                .expect("members"),
            [second_member]
        );
    }

    #[test]
    fn keep_tracking_uses_accumulated_members_for_budget_and_commit_counts() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let first_title = PageTitle::new("Retained first").expect("title");
        let second_title = PageTitle::new("Retained second").expect("title");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([first_title.clone(), second_title.clone()]).expect("selection"),
        );
        let collection_id = library
            .create_collection(
                wiki_id,
                "Keep tracking budget",
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited()
                    .with_maximum_pages(1)
                    .expect("budget"),
                CollectionRemovalPolicy::KeepTracking,
            )
            .expect("create collection");
        let first = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page ID"),
            namespace: 0,
            title: first_title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(first_title),
        };
        let second = ResolvedCollectionMember {
            page_id: PageId::new(11).expect("page ID"),
            namespace: 0,
            title: second_title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(second_title),
        };
        assert_eq!(
            library
                .commit_resolved_membership(collection_id, std::slice::from_ref(&first))
                .expect("first preview")
                .active_members,
            1
        );
        let generation_before_failure = library
            .collection(collection_id)
            .expect("collection")
            .expect("present")
            .generation;
        assert!(matches!(
            library.commit_resolved_membership(collection_id, std::slice::from_ref(&second)),
            Err(StoreError::CollectionBudgetExceeded {
                resource: "pages",
                limit: 1,
                estimated: 2,
            })
        ));
        assert_eq!(
            library
                .resolved_collection_members(collection_id)
                .expect("rolled back membership"),
            std::slice::from_ref(&first)
        );
        assert_eq!(
            library
                .collection(collection_id)
                .expect("collection")
                .expect("present")
                .generation,
            generation_before_failure
        );

        library
            .set_collection_configuration(
                collection_id,
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited()
                    .with_maximum_pages(2)
                    .expect("budget"),
                CollectionRemovalPolicy::KeepTracking,
            )
            .expect("raise budget");
        assert_eq!(
            library
                .commit_resolved_membership(collection_id, std::slice::from_ref(&second))
                .expect("disjoint preview after raising budget"),
            MembershipCommit {
                active_members: 2,
                removed_members: 0,
            }
        );
    }

    #[test]
    fn collection_generation_tracks_mutations_but_not_schedule_only_edits() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Generation")
            .expect("create collection");
        let generation = |library: &Library| {
            library
                .collection(collection_id)
                .expect("collection")
                .expect("present")
                .generation
        };
        assert_eq!(generation(&library), 1);
        let missing = PageTitle::new("Missing generation page").expect("title");
        library
            .record_missing_title(collection_id, &missing, 0)
            .expect("record missing title");
        assert_eq!(generation(&library), 2);
        library
            .rename_collection(collection_id, "Renamed generation")
            .expect("rename");
        assert_eq!(generation(&library), 3);

        let title = PageTitle::new("Generation page").expect("title");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([title.clone()]).expect("selection"),
        );
        library
            .set_collection_configuration(
                collection_id,
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited(),
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect("configuration");
        assert_eq!(generation(&library), 4);
        let member = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page ID"),
            namespace: 0,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title),
        };
        library
            .commit_resolved_membership(collection_id, std::slice::from_ref(&member))
            .expect("membership");
        assert_eq!(generation(&library), 5);
        library
            .set_collection_schedule(
                collection_id,
                ScheduleCadence::interval(60).expect("cadence"),
                0,
                false,
                Some(100),
            )
            .expect("schedule");
        assert_eq!(generation(&library), 5);
        library
            .tombstone_collection(collection_id)
            .expect("tombstone");
        assert_eq!(generation(&library), 6);
    }

    #[test]
    fn legacy_create_collection_rolls_back_the_draft_when_configuration_fails() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let title = PageTitle::new("Atomic legacy create").expect("title");
        let rule = CollectionRule::ExplicitTitles(TitleSelection::new([title]).expect("selection"));
        let error = library
            .create_collection(
                wiki_id,
                "Must roll back",
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited()
                    .with_maximum_bytes(u64::MAX)
                    .expect("domain budget"),
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect_err("SQLite-unrepresentable budget");
        assert!(matches!(error, StoreError::IntegerOutOfRange(u64::MAX)));
        assert!(library.collections().expect("collections").is_empty());
        assert_eq!(table_count(&library, "collection_configuration"), 0);
        assert_eq!(table_count(&library, "collection_schedules"), 0);
        assert_eq!(table_count(&library, "collection_rule_titles"), 0);
    }

    #[test]
    fn tombstone_stops_tracking_and_preserves_run_manifest_and_canonical_history() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let title = PageTitle::new("Retained page").expect("title");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([title.clone()]).expect("selection"),
        );
        let member = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page ID"),
            namespace: 0,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title.clone()),
        };
        let preview = CollectionPreviewCommit {
            rule: &rule,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            members: std::slice::from_ref(&member),
            missing_titles: &[],
            predicted_canonical_bytes: Some(64),
        };
        let (collection_id, _) = library
            .create_collection_from_preview(wiki_id, "Retained", preview)
            .expect("create collection");
        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            10,
            100,
            "2026-08-21T10:00:00Z",
            "Retained page",
        );
        let completed_run = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("start completed run")
            .status
            .run_id;
        library
            .complete_sync_run(completed_run, None)
            .expect("complete run");
        let manifest = library
            .append_sync_manifest(completed_run)
            .expect("append manifest");
        assert_eq!(manifest.manifest.collection_id, Some(collection_id));
        let running_run = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Update, 200)
            .expect("start running update")
            .status
            .run_id;
        library
            .set_collection_schedule(
                collection_id,
                ScheduleCadence::interval(60).expect("cadence"),
                0,
                false,
                Some(1),
            )
            .expect("due schedule");
        let object_count = table_count(&library, "content_objects");
        let revision_count = table_count(&library, "revisions");

        library
            .tombstone_collection(collection_id)
            .expect("tombstone");
        let tombstone = library
            .collection(collection_id)
            .expect("collection")
            .expect("retained tombstone");
        assert_eq!(tombstone.status, CollectionStatus::Tombstoned);
        assert!(tombstone.tombstoned_at.is_some());
        assert_eq!(tombstone.page_count, 0);
        assert!(
            library
                .collections()
                .expect("active collections")
                .is_empty()
        );
        assert_eq!(
            library
                .collections_including_tombstones()
                .expect("audit collections"),
            std::slice::from_ref(&tombstone)
        );
        assert!(
            library
                .due_schedules(10, 10)
                .expect("due schedules")
                .is_empty()
        );
        assert!(
            library
                .collection_schedule(collection_id)
                .expect("schedule")
                .expect("retained schedule")
                .paused
        );
        assert_eq!(
            library
                .sync_run_status(running_run)
                .expect("run status")
                .expect("retained run")
                .state,
            SyncRunState::Cancelled
        );
        assert_eq!(
            library
                .sync_run_status(completed_run)
                .expect("completed status")
                .expect("retained run")
                .collection_id,
            Some(collection_id)
        );
        assert_eq!(table_count(&library, "content_objects"), object_count);
        assert_eq!(table_count(&library, "revisions"), revision_count);
        assert_eq!(table_count(&library, "collection_resolved_members"), 1);
        assert_eq!(table_count(&library, "collection_pages"), 0);
        assert_eq!(
            library
                .validated_manifest_chain()
                .expect("retained manifest")[0]
                .manifest
                .collection_id,
            Some(collection_id)
        );
        assert!(matches!(
            library.update_collection_from_preview(
                collection_id,
                tombstone.generation,
                None,
                preview,
            ),
            Err(StoreError::CollectionTombstoned(id)) if id == collection_id
        ));
        assert!(matches!(
            library.start_or_resume_sync_run(
                wiki_id,
                Some(collection_id),
                SyncRunKind::Update,
                300,
            ),
            Err(StoreError::CollectionTombstoned(id)) if id == collection_id
        ));
        library
            .tombstone_collection(collection_id)
            .expect("idempotent tombstone");
        assert_eq!(
            library
                .collection(collection_id)
                .expect("collection")
                .expect("tombstone")
                .tombstoned_at,
            tombstone.tombstoned_at
        );
    }

    #[test]
    fn tombstoned_partial_run_remains_hash_verifiable_without_becoming_manifest_evidence() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let title = PageTitle::new("Partially durable page").expect("title");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([title.clone()]).expect("selection"),
        );
        let member = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page ID"),
            namespace: 0,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title),
        };
        let preview = CollectionPreviewCommit {
            rule: &rule,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            members: std::slice::from_ref(&member),
            missing_titles: &[],
            predicted_canonical_bytes: None,
        };
        let (collection_id, _) = library
            .create_collection_from_preview(wiki_id, "Partial run", preview)
            .expect("create collection");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("start run")
            .status
            .run_id;
        library
            .enqueue_sync_job(run_id, "capture:10", "capture-current", Some("10"))
            .expect("queue job");
        library
            .claim_next_sync_job(run_id)
            .expect("claim job")
            .expect("running job");
        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            10,
            100,
            "2026-08-21T10:00:00Z",
            "Partially durable page",
        );

        library
            .tombstone_collection(collection_id)
            .expect("tombstone partial run");
        assert_eq!(
            library
                .sync_run_status(run_id)
                .expect("run status")
                .expect("retained cancelled run")
                .state,
            SyncRunState::Cancelled
        );
        assert!(matches!(
            library.append_sync_manifest(run_id),
            Err(StoreError::SyncRunNotSucceeded(id)) if id == run_id
        ));
        assert!(
            library
                .unmanifested_succeeded_run_ids(10)
                .expect("manifest coverage candidates")
                .is_empty()
        );
        assert!(
            library
                .validated_manifest_chain()
                .expect("manifest inventory")
                .is_empty()
        );

        let objects = library
            .logical_objects_after(None, 100)
            .expect("logical object inventory");
        assert_eq!(objects.len(), 1);
        assert_eq!(
            library
                .read_object(objects[0].object.id)
                .expect("full hash-verified read"),
            b"Partially durable page"
        );
        let metadata = library
            .integrity_metadata_records_after(None, 100)
            .expect("full metadata verification page");
        assert_eq!(
            u64::try_from(metadata.len()).expect("metadata count"),
            library
                .integrity_metadata_record_count()
                .expect("expected metadata count")
        );
        assert!(metadata.iter().all(|record| record.issues.is_empty()));
    }

    #[test]
    fn purge_preview_requires_tombstone_and_excludes_shared_references() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let target = library
            .create_explicit_collection(wiki_id, "Target archive")
            .expect("target collection");
        let retained = library
            .create_explicit_collection(wiki_id, "Retained archive")
            .expect("retained collection");
        let exclusive = capture_test_page_source(
            &mut library,
            wiki_id,
            target,
            10,
            100,
            "Exclusive page",
            b"exclusive payload",
        );
        let duplicated = capture_test_page_source(
            &mut library,
            wiki_id,
            target,
            11,
            110,
            "Target duplicate",
            b"shared bytes",
        );
        assert_eq!(
            duplicated,
            capture_test_page_source(
                &mut library,
                wiki_id,
                retained,
                20,
                200,
                "Retained duplicate",
                b"shared bytes",
            )
        );
        capture_test_page_source(
            &mut library,
            wiki_id,
            target,
            30,
            300,
            "Shared page",
            b"shared page payload",
        );
        capture_test_page_source(
            &mut library,
            wiki_id,
            retained,
            30,
            300,
            "Shared page",
            b"shared page payload",
        );

        assert!(matches!(
            library.preview_collection_purge(target),
            Err(StoreError::CollectionMustBeTombstoned(id)) if id == target
        ));
        library
            .tombstone_collection(target)
            .expect("tombstone target");
        let preview = library
            .preview_collection_purge(target)
            .expect("exclusive purge preview");
        assert_eq!(preview.object_count, 1);
        assert_eq!(preview.wikitext_object_count, 1);
        assert_eq!(preview.media_object_count, 0);
        assert_eq!(preview.logical_bytes, b"exclusive payload".len() as u64);
        assert_eq!(preview.loose_object_count, 1);
        assert_eq!(preview.affected_pack_count, 0);
        assert_eq!(preview.collection_name, "Target archive");
        assert!(preview.tombstoned_at > 0);
        assert!(
            preview.fingerprint.parse::<ManifestId>().is_ok(),
            "preview fingerprint is a canonical BLAKE3 identity"
        );
        assert_eq!(
            library.read_object(exclusive).expect("exclusive bytes"),
            b"exclusive payload"
        );
        assert_eq!(
            library.read_object(duplicated).expect("retained bytes"),
            b"shared bytes"
        );
    }

    #[test]
    fn purge_preview_excludes_source_wide_manifest_text_and_media_claims() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let target = library
            .create_explicit_collection(wiki_id, "Manifest-protected target")
            .expect("target collection");
        capture_test_page_source(
            &mut library,
            wiki_id,
            target,
            60,
            600,
            "Manifest protected page",
            b"manifest protected text",
        );
        let file_title = PageTitle::new("File:Manifest-protected.png").expect("file title");
        library
            .capture_revision_thumbnail(
                wiki_id,
                PageId::new(60).expect("page ID"),
                RevisionId::new(600).expect("revision ID"),
                ThumbnailPolicy::new(640, 8, 1024).expect("thumbnail policy"),
                &ThumbnailCapture {
                    media_id: MediaId::new(9060).expect("media ID"),
                    file_title: &file_title,
                    source_sha1: "abcdef0123456789abcdef0123456789",
                    original_url: "https://upload.wikimedia.org/manifest-protected.png",
                    description_url:
                        "https://commons.wikimedia.org/wiki/File:Manifest-protected.png",
                    author: "Fixture photographer",
                    attribution: "Fixture photographer / Wikimedia Commons",
                    license_name: "CC BY-SA 4.0",
                    license_url: Some("https://creativecommons.org/licenses/by-sa/4.0/"),
                    width: 1,
                    height: 1,
                    mime_type: ThumbnailMimeType::Png,
                    captured_at: 1_776_000_000,
                    source: VALID_PNG,
                },
                RevisionMediaPlacement {
                    index: 0,
                    kind: MediaPlacementKind::Lead,
                    caption: Some("Manifest-protected media"),
                    alt_text: Some("Fixture alternative"),
                },
            )
            .expect("capture media");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 100)
            .expect("source-wide run")
            .status
            .run_id;
        library
            .complete_sync_run(run_id, None)
            .expect("complete source-wide run");
        let manifest = library.append_sync_manifest(run_id).expect("manifest");
        assert_eq!(manifest.manifest.collection_id, None);
        assert_eq!(manifest.manifest.introduced_revisions.len(), 1);
        assert_eq!(
            manifest
                .manifest
                .media_snapshot
                .as_ref()
                .expect("media snapshot")
                .inventory
                .len(),
            1
        );
        library
            .tombstone_collection(target)
            .expect("tombstone target");
        assert!(matches!(
            library.preview_collection_purge(target),
            Err(StoreError::NoExclusivePurgePayload(id)) if id == target
        ));
    }

    #[test]
    fn purge_authorization_is_stale_safe_idempotent_bounded_and_non_destructive() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let target = library
            .create_explicit_collection(wiki_id, "Confirmed target")
            .expect("target collection");
        let retained = library
            .create_explicit_collection(wiki_id, "Pack neighbor")
            .expect("retained collection");
        let target_object = capture_test_page_source(
            &mut library,
            wiki_id,
            target,
            40,
            400,
            "Target packed page",
            b"target packed payload",
        );
        capture_test_page_source(
            &mut library,
            wiki_id,
            retained,
            50,
            500,
            "Retained packed page",
            b"retained packed payload",
        );
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(target), SyncRunKind::Bootstrap, 100)
            .expect("start run")
            .status
            .run_id;
        library
            .complete_sync_run(run_id, None)
            .expect("complete run");
        let manifest = library.append_sync_manifest(run_id).expect("manifest");
        library
            .tombstone_collection(target)
            .expect("tombstone target");

        let stale = library
            .preview_collection_purge(target)
            .expect("loose preview");
        assert_eq!(
            stale.manifest_head_sequence,
            Some(manifest.manifest.sequence)
        );
        assert_eq!(stale.manifest_head_id, Some(manifest.id));
        let pack = library
            .pack_loose_objects()
            .expect("pack candidates")
            .expect("new pack");
        assert!(matches!(
            library.authorize_collection_purge(
                target,
                PurgeAuthorization {
                    collection_name: &stale.collection_name,
                    preview_fingerprint: &stale.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                }
            ),
            Err(StoreError::StalePurgePreview(id)) if id == target
        ));
        assert_eq!(table_count(&library, "purge_operations"), 0);

        let preview = library
            .preview_collection_purge(target)
            .expect("packed preview");
        assert_eq!(preview.affected_pack_count, 1);
        assert_eq!(preview.whole_pack_count, 0);
        assert_eq!(preview.mixed_pack_count, 1);
        assert_eq!(preview.loose_object_count, 1);
        assert!(matches!(
            library.authorize_collection_purge(
                target,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: false,
                    backups_not_erased_acknowledged: true,
                }
            ),
            Err(StoreError::PurgeAcknowledgementsRequired)
        ));

        let authorization = PurgeAuthorization {
            collection_name: &preview.collection_name,
            preview_fingerprint: &preview.fingerprint,
            payload_only_acknowledged: true,
            backups_not_erased_acknowledged: true,
        };
        let receipt = library
            .authorize_collection_purge(target, authorization)
            .expect("authorize purge");
        let repeated = library
            .authorize_collection_purge(target, authorization)
            .expect("idempotent authorization");
        assert_eq!(repeated, receipt);
        library
            .connection()
            .execute(
                "UPDATE content_objects SET verification_state = 'corrupt'
                 WHERE object_id = ?1",
                [target_object.to_string()],
            )
            .expect("change logical verification state");
        assert!(matches!(
            library.authorize_collection_purge(target, authorization),
            Err(StoreError::StalePurgePreview(id)) if id == target
        ));
        library
            .connection()
            .execute(
                "UPDATE content_objects SET verification_state = 'verified'
                 WHERE object_id = ?1",
                [target_object.to_string()],
            )
            .expect("restore logical verification state");
        assert_eq!(table_count(&library, "purge_operations"), 1);
        assert_eq!(table_count(&library, "purge_objects"), 1);
        assert_eq!(table_count(&library, "purge_pack_work"), 1);
        let objects = library
            .purge_objects_after(receipt.purge_id, None, 1)
            .expect("purge object page");
        assert_eq!(objects[0].object.id, target_object);
        assert!(matches!(
            library.purge_objects_after(receipt.purge_id, None, 0),
            Err(StoreError::InvalidConfig(_))
        ));
        assert_eq!(
            library
                .read_object(target_object)
                .expect("payload retained"),
            b"target packed payload"
        );
        assert!(library.contains(target_object).expect("location retained"));
        assert_eq!(
            library
                .connection()
                .query_row(
                    "SELECT state FROM packs WHERE pack_id = ?1",
                    [&pack.pack_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("pack state"),
            "verified"
        );
    }

    #[test]
    fn used_source_removal_fails_while_empty_source_removal_succeeds() {
        let (_directory, mut library) = test_library();
        let used_wiki = library
            .register_wiki("https://used.example/w/api.php", "used")
            .expect("used wiki");
        let empty_wiki = library
            .register_wiki("https://empty.example/w/api.php", "empty")
            .expect("empty wiki");
        let collection_id = library
            .create_explicit_collection(used_wiki, "Retained evidence")
            .expect("collection");
        capture_test_page(
            &mut library,
            used_wiki,
            collection_id,
            10,
            20,
            "2026-08-21T10:00:00Z",
            "Retained page",
        );
        let run_id = library
            .start_or_resume_sync_run(used_wiki, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("start run")
            .status
            .run_id;
        library
            .complete_sync_run(run_id, None)
            .expect("complete run");
        library.append_sync_manifest(run_id).expect("manifest");

        assert!(matches!(
            library.remove_wiki(used_wiki),
            Err(StoreError::WikiInUse {
                wiki_id,
                collections: 1,
                captured_pages: 1,
                sync_runs: 1,
                checkpoints: 1,
                manifests: 1,
            }) if wiki_id == used_wiki
        ));
        assert!(
            library
                .wiki(used_wiki)
                .expect("used source query")
                .is_some()
        );
        assert!(
            library
                .revision(used_wiki, RevisionId::new(20).expect("revision ID"))
                .expect("retained revision")
                .is_some()
        );
        assert_eq!(library.manifest_count().expect("retained manifest"), 1);

        library
            .remove_wiki(empty_wiki)
            .expect("remove empty source");
        assert!(
            library
                .wiki(empty_wiki)
                .expect("removed source query")
                .is_none()
        );
        assert!(matches!(
            library.remove_wiki(empty_wiki),
            Err(StoreError::WikiNotFound(wiki_id)) if wiki_id == empty_wiki
        ));
    }

    #[test]
    fn title_list_rules_and_logical_object_pagination_are_durable_and_bounded() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let titles =
            TitleSelection::from_newline_delimited("Rust\nFerris\nRust\n").expect("title list");
        let collection_id = library
            .create_collection(
                wiki_id,
                "Imported",
                &CollectionRule::TitleList(titles.clone()),
                HistoryPolicy::Since(UnixTimestamp::from_seconds(123)),
                CollectionBudget::unlimited(),
                CollectionRemovalPolicy::KeepTracking,
            )
            .expect("create title-list collection");
        let stored = library
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured");
        assert_eq!(stored.rule, CollectionRule::TitleList(titles));
        assert_eq!(
            stored.history_policy,
            HistoryPolicy::Since(UnixTimestamp::from_seconds(123))
        );

        library
            .put_bytes(ObjectKind::Wikitext, b"first")
            .expect("first object");
        library
            .put_bytes(ObjectKind::Media, b"second")
            .expect("second object");
        assert_eq!(library.logical_object_count().expect("object count"), 2);
        let first_page = library.logical_objects_after(None, 1).expect("first page");
        assert_eq!(first_page.len(), 1);
        let second_page = library
            .logical_objects_after(Some(first_page[0].object.id), 1)
            .expect("second page");
        assert_eq!(second_page.len(), 1);
        assert!(first_page[0].object.id < second_page[0].object.id);
        assert!(matches!(
            library.logical_objects_after(None, 0),
            Err(StoreError::InvalidConfig(_))
        ));
    }

    #[test]
    fn failed_bounded_stream_never_creates_metadata() {
        let directory = tempfile::tempdir().expect("temporary library");
        let config = StoreConfig::default()
            .with_max_object_bytes(8)
            .expect("valid limit");
        let mut library =
            Library::open_with_config(directory.path(), config).expect("open library");

        assert!(matches!(
            library.put_reader(ObjectKind::Wikitext, 4, b"too long".as_slice()),
            Err(StoreError::LengthMismatch {
                expected: 4,
                actual: 8
            })
        ));
        assert!(matches!(
            library.put_reader(ObjectKind::Wikitext, 9, b"ignored".as_slice()),
            Err(StoreError::ObjectTooLarge {
                limit: 8,
                actual: 9
            })
        ));
        assert_eq!(table_count(&library, "content_objects"), 0);
    }

    #[test]
    fn corrupt_loose_object_is_rejected() {
        let (_directory, mut library) = test_library();
        let stored = library
            .put_bytes(ObjectKind::Wikitext, b"canonical")
            .expect("store object");
        let relative: String = library
            .connection()
            .query_row(
                "SELECT relative_path FROM object_locations WHERE object_id = ?1",
                [stored.id.to_string()],
                |row| row.get(0),
            )
            .expect("object path");
        fs::write(library.root().join(relative), b"not zstd").expect("tamper object");

        assert!(matches!(
            library.read_object(stored.id),
            Err(StoreError::Io(_))
        ));
    }

    fn evolving_objects(library: &mut Library, count: usize) -> Vec<(ObjectId, Vec<u8>)> {
        let mut bytes = Vec::with_capacity(32 * 1024);
        let mut state = 0x9e37_79b9_u32;
        for _ in 0..32 * 1024 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            bytes.push(state as u8);
        }
        let mut stored = Vec::with_capacity(count);
        for version in 0..count {
            let offset = 512 + version * 997;
            for (index, byte) in bytes[offset..offset + 32].iter_mut().enumerate() {
                *byte = (version as u8).wrapping_mul(31).wrapping_add(index as u8);
            }
            let object = library
                .put_bytes(ObjectKind::Wikitext, &bytes)
                .expect("store evolving object");
            stored.push((object.id, bytes.clone()));
        }
        stored
    }

    fn pack_revision_bytes(page_id: u64, version: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 * 1024);
        let mut state = (page_id as u32).wrapping_mul(0x9e37_79b9);
        for _ in 0..8 * 1024 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            bytes.push(state as u8);
        }
        for edit in 1..=version {
            let offset = 256 + edit * 337;
            for (index, byte) in bytes[offset..offset + 32].iter_mut().enumerate() {
                *byte = (page_id as u8)
                    .wrapping_add((edit as u8).wrapping_mul(17))
                    .wrapping_add(index as u8);
            }
        }
        bytes
    }

    fn capture_interleaved_pack_fixture(
        library: &mut Library,
        page_order: &[u64],
        versions: usize,
    ) {
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register pack fixture wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Pack fixture")
            .expect("create pack fixture collection");
        for &page in page_order {
            let title =
                PageTitle::new(format!("Pack fixture page {page}")).expect("pack fixture title");
            let source = pack_revision_bytes(page, 0);
            library
                .capture_current_revision(
                    wiki_id,
                    collection_id,
                    &CurrentRevisionCapture {
                        page_id: PageId::new(page).expect("pack fixture page ID"),
                        namespace: 0,
                        title: &title,
                        revision_id: RevisionId::new(page * 100).expect("pack fixture revision ID"),
                        parent_id: None,
                        timestamp: "2026-08-01T00:00:00Z",
                        author: None,
                        author_id: None,
                        comment: None,
                        minor: false,
                        upstream_sha1: None,
                        content_model: "wikitext",
                        source: &source,
                    },
                )
                .expect("capture pack fixture head");
        }
        for version in 1..versions {
            let timestamp = format!("2026-08-{:02}T00:00:00Z", version + 1);
            for &page in page_order {
                let source = pack_revision_bytes(page, version);
                library
                    .capture_revision(
                        wiki_id,
                        PageId::new(page).expect("pack fixture page ID"),
                        &RevisionCapture {
                            revision_id: RevisionId::new(page * 100 + version as u64)
                                .expect("pack fixture revision ID"),
                            parent_id: Some(
                                RevisionId::new(page * 100 + version as u64 - 1)
                                    .expect("pack fixture parent revision ID"),
                            ),
                            timestamp: &timestamp,
                            author: None,
                            author_id: None,
                            comment: None,
                            minor: false,
                            upstream_sha1: None,
                            content_model: "wikitext",
                            source: &source,
                        },
                    )
                    .expect("capture pack fixture history");
            }
        }
    }

    #[test]
    fn pack_tuning_groups_page_history_and_reduces_fixture_storage() {
        let (_directory, mut library) = test_library();
        let pages = (1..=20).collect::<Vec<_>>();
        capture_interleaved_pack_fixture(&mut library, &pages, 5);
        let loose_bytes: u64 = library
            .connection()
            .query_row(
                "SELECT SUM(compressed_length) FROM object_locations
                 WHERE storage_kind = 'loose' AND verification_state = 'verified'",
                [],
                |row| row.get(0),
            )
            .expect("loose fixture bytes");

        let summary = library
            .pack_loose_objects()
            .expect("pack tuned fixture")
            .expect("nonempty tuned pack");
        assert_eq!(summary.object_count, 100);
        assert!(summary.full_entries >= pages.len() as u64);
        assert!(summary.delta_entries >= 75);
        assert!(summary.pack_bytes + summary.index_bytes < loose_bytes / 2);

        let physical_pages = library
            .connection()
            .prepare(
                "SELECT revisions.page_id
                 FROM object_locations AS locations
                 JOIN revisions ON revisions.content_object_id = locations.object_id
                 WHERE locations.pack_id = ?1
                 ORDER BY locations.pack_offset",
            )
            .expect("prepare physical page order")
            .query_map([&summary.pack_id], |row| row.get::<_, u64>(0))
            .expect("query physical page order")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect physical page order");
        let mut completed_pages = HashSet::new();
        for pair in physical_pages.windows(2) {
            if pair[0] != pair[1] {
                assert!(completed_pages.insert(pair[0]));
                assert!(!completed_pages.contains(&pair[1]));
            }
        }
    }

    #[test]
    fn pack_layout_is_deterministic_across_ingestion_orders() {
        let first_directory = tempfile::tempdir().expect("first pack library");
        let second_directory = tempfile::tempdir().expect("second pack library");
        let mut first = Library::open(first_directory.path()).expect("open first pack library");
        let mut second = Library::open(second_directory.path()).expect("open second pack library");
        capture_interleaved_pack_fixture(&mut first, &[1, 2, 3, 4], 5);
        capture_interleaved_pack_fixture(&mut second, &[4, 2, 1, 3], 5);

        let first_summary = first
            .pack_loose_objects()
            .expect("pack first order")
            .expect("first pack");
        let second_summary = second
            .pack_loose_objects()
            .expect("pack second order")
            .expect("second pack");
        assert_eq!(first_summary.pack_id, second_summary.pack_id);
        assert_eq!(first_summary.pack_bytes, second_summary.pack_bytes);
        assert_eq!(first_summary.index_bytes, second_summary.index_bytes);
        assert_eq!(first_summary.full_entries, second_summary.full_entries);
        assert_eq!(first_summary.delta_entries, second_summary.delta_entries);
    }

    #[test]
    fn oversized_loose_object_does_not_block_smaller_pack_candidates() {
        let directory = tempfile::tempdir().expect("temporary library");
        let config = StoreConfig::default()
            .with_max_pack_input_bytes(256)
            .expect("small pack input bound");
        let mut library =
            Library::open_with_config(directory.path(), config).expect("open bounded library");
        let oversized = library
            .put_bytes(ObjectKind::Wikitext, &vec![0x55; 512])
            .expect("store oversized pack candidate");
        let eligible = library
            .put_bytes(ObjectKind::Wikitext, &[0x33; 128])
            .expect("store eligible pack candidate");

        let summary = library
            .pack_loose_objects()
            .expect("pack smaller candidate")
            .expect("eligible pack");
        assert_eq!(summary.object_count, 1);
        assert_eq!(
            library.read_object(oversized.id).expect("read oversized"),
            vec![0x55; 512]
        );
        assert_eq!(
            library.read_object(eligible.id).expect("read eligible"),
            vec![0x33; 128]
        );
        assert!(
            library
                .pack_loose_objects()
                .expect("repeat pack scan")
                .is_none()
        );
    }

    #[test]
    fn verified_pack_round_trips_full_and_bounded_delta_entries_after_pruning() {
        let (directory, mut library) = test_library();
        let objects = evolving_objects(&mut library, 20);

        let summary = library
            .pack_loose_objects()
            .expect("build pack")
            .expect("nonempty pack");
        assert_eq!(summary.object_count, objects.len() as u64);
        assert!(summary.full_entries >= 1);
        assert!(summary.delta_entries >= 1);
        let maximum_depth: i64 = library
            .connection()
            .query_row(
                "SELECT MAX(delta_depth) FROM object_locations WHERE pack_id = ?1",
                [&summary.pack_id],
                |row| row.get(0),
            )
            .expect("maximum delta depth");
        assert!(maximum_depth <= i64::from(MAX_DELTA_DEPTH));

        assert_eq!(
            library
                .prune_packed_loose_objects(&summary.pack_id)
                .expect("prune loose copies"),
            objects.len() as u64
        );
        for (id, expected) in &objects {
            assert_eq!(
                library.read_object(*id).expect("read packed object"),
                *expected
            );
        }
        assert!(
            library
                .pack_loose_objects()
                .expect("repeat pack scan")
                .is_none()
        );

        drop(library);
        let reopened = Library::open(directory.path()).expect("reopen packed library");
        for (id, expected) in objects {
            assert_eq!(
                reopened.read_object(id).expect("read after reopen"),
                expected
            );
        }
    }

    #[test]
    fn corrupt_pack_index_falls_back_to_loose_then_fails_after_pruning() {
        let (_directory, mut library) = test_library();
        let object = library
            .put_bytes(ObjectKind::Wikitext, b"pack index fallback")
            .expect("store object");
        let summary = library
            .pack_loose_objects()
            .expect("build pack")
            .expect("nonempty pack");
        let index_path: String = library
            .connection()
            .query_row(
                "SELECT index_path FROM packs WHERE pack_id = ?1",
                [&summary.pack_id],
                |row| row.get(0),
            )
            .expect("index path");
        let index_path = library.root().join(index_path);
        let original_index = fs::read(&index_path).expect("read original index");
        fs::write(&index_path, b"tampered index").expect("tamper index");

        assert_eq!(
            library.read_object(object.id).expect("loose fallback"),
            b"pack index fallback"
        );
        assert!(matches!(
            library.prune_packed_loose_objects(&summary.pack_id),
            Err(StoreError::CorruptPack("pack index checksum mismatch"))
        ));
        fs::write(&index_path, &original_index).expect("restore index");
        library
            .prune_packed_loose_objects(&summary.pack_id)
            .expect("prune loose copy");
        fs::write(&index_path, b"tampered index").expect("tamper packed-only index");
        assert!(matches!(
            library.read_object(object.id),
            Err(StoreError::CorruptPack("pack index checksum mismatch"))
        ));
    }

    #[test]
    fn repacking_preserves_ids_and_bytes_before_the_old_pack_is_retired() {
        let (_directory, mut library) = test_library();
        let objects = evolving_objects(&mut library, 12);
        let first = library
            .pack_loose_objects()
            .expect("first pack")
            .expect("nonempty first pack");
        library
            .prune_packed_loose_objects(&first.pack_id)
            .expect("prune first loose copies");

        let second = library.repack_pack(&first.pack_id).expect("repack");
        assert_ne!(second.pack_id, first.pack_id);
        assert_eq!(second.object_count, first.object_count);
        assert_eq!(
            library
                .retire_pack(&first.pack_id)
                .expect("retire first pack"),
            objects.len() as u64
        );
        for (id, expected) in objects {
            assert_eq!(
                library.read_object(id).expect("read repacked object"),
                expected
            );
        }
    }

    #[test]
    fn tampered_pack_payload_is_rejected_without_a_loose_copy() {
        use std::fs::OpenOptions;

        let (_directory, mut library) = test_library();
        let object = library
            .put_bytes(ObjectKind::Wikitext, b"tamper the immutable pack payload")
            .expect("store object");
        let summary = library
            .pack_loose_objects()
            .expect("build pack")
            .expect("nonempty pack");
        library
            .prune_packed_loose_objects(&summary.pack_id)
            .expect("prune loose copy");
        let (pack_path, offset, length): (String, i64, i64) = library
            .connection()
            .query_row(
                "SELECT packs.pack_path, locations.pack_offset,
                        locations.compressed_length
                 FROM object_locations AS locations
                 JOIN packs USING (pack_id)
                 WHERE locations.pack_id = ?1 AND locations.object_id = ?2",
                params![summary.pack_id, object.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("pack entry location");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(library.root().join(pack_path))
            .expect("open pack for tamper");
        file.seek(SeekFrom::Start((offset + length - 1) as u64))
            .expect("seek payload byte");
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).expect("read payload byte");
        file.seek(SeekFrom::Current(-1))
            .expect("rewind payload byte");
        byte[0] ^= 0xff;
        file.write_all(&byte).expect("tamper payload byte");

        assert!(library.read_object(object.id).is_err());
    }

    #[test]
    fn pack_database_pointer_is_checked_against_the_index() {
        let (_directory, mut library) = test_library();
        let object = library
            .put_bytes(ObjectKind::Wikitext, b"pointer integrity")
            .expect("store object");
        let summary = library
            .pack_loose_objects()
            .expect("build pack")
            .expect("nonempty pack");
        library
            .prune_packed_loose_objects(&summary.pack_id)
            .expect("prune loose copy");
        library
            .connection()
            .execute(
                "UPDATE object_locations SET pack_offset = pack_offset + 1
                 WHERE pack_id = ?1 AND object_id = ?2",
                params![summary.pack_id, object.id.to_string()],
            )
            .expect("tamper database pointer");

        assert!(matches!(
            library.read_object(object.id),
            Err(StoreError::CorruptMetadata(
                "pack offset disagrees with index"
            ))
        ));
    }

    #[test]
    fn pack_activation_can_restart_after_durable_files_lose_metadata() {
        let (_directory, mut library) = test_library();
        let objects = evolving_objects(&mut library, 3);
        let first = library
            .pack_loose_objects()
            .expect("first pack")
            .expect("nonempty pack");
        library
            .connection()
            .execute(
                "DELETE FROM object_locations WHERE pack_id = ?1",
                [&first.pack_id],
            )
            .expect("remove interrupted locations");
        library
            .connection()
            .execute("DELETE FROM packs WHERE pack_id = ?1", [&first.pack_id])
            .expect("remove interrupted activation");

        let restarted = library
            .pack_loose_objects()
            .expect("restart pack")
            .expect("recreated pack");
        assert_eq!(restarted.pack_id, first.pack_id);
        assert_eq!(restarted.generation, first.generation);
        library
            .prune_packed_loose_objects(&restarted.pack_id)
            .expect("prune loose copies");
        for (id, expected) in objects {
            assert_eq!(
                library.read_object(id).expect("read restarted pack"),
                expected
            );
        }
    }

    #[test]
    fn pack_creation_respects_configured_object_count_bound() {
        let directory = tempfile::tempdir().expect("temporary library");
        let config = StoreConfig::default()
            .with_max_pack_objects(3)
            .expect("valid pack bound");
        let mut library =
            Library::open_with_config(directory.path(), config).expect("open library");
        evolving_objects(&mut library, 7);

        let first = library
            .pack_loose_objects()
            .expect("first pack")
            .expect("nonempty first pack");
        let second = library
            .pack_loose_objects()
            .expect("second pack")
            .expect("nonempty second pack");
        let third = library
            .pack_loose_objects()
            .expect("third pack")
            .expect("nonempty third pack");
        assert_eq!(
            (first.object_count, second.object_count, third.object_count),
            (3, 3, 1)
        );
    }

    #[test]
    fn existing_durable_file_can_be_adopted_after_metadata_loss() {
        let (directory, mut library) = test_library();
        let bytes = b"durable before metadata";
        let stored = library
            .put_bytes(ObjectKind::Wikitext, bytes)
            .expect("store object");
        library
            .connection()
            .execute("DELETE FROM content_objects", [])
            .expect("simulate lost transaction");
        drop(library);

        let mut reopened = Library::open(directory.path()).expect("reopen library");
        let adopted = reopened
            .put_bytes(ObjectKind::Wikitext, bytes)
            .expect("adopt object");
        assert_eq!(adopted.id, stored.id);
        assert_eq!(
            reopened.read_object(adopted.id).expect("read adopted"),
            bytes
        );
    }

    #[test]
    fn current_revision_capture_is_idempotent_and_preserves_identity() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Systems languages")
            .expect("create collection");
        let title = PageTitle::new("Rust (programming language)").expect("title");
        let page_id = PageId::new(25_357_340).expect("page ID");
        let revision_id = RevisionId::new(1_300_000_001).expect("revision ID");
        let capture = CurrentRevisionCapture {
            page_id,
            namespace: 0,
            title: &title,
            revision_id,
            parent_id: Some(RevisionId::new(1_300_000_000).expect("parent ID")),
            timestamp: "2026-08-19T12:34:56Z",
            author: Some("Fixture editor"),
            author_id: Some(42),
            comment: Some("Improve the history section"),
            minor: true,
            upstream_sha1: Some("mz6rzjalvs99ygh9s19aseznld8m1pu"),
            content_model: "wikitext",
            source: b"== Rust ==\nA systems programming language.",
        };

        let first = library
            .capture_current_revision(wiki_id, collection_id, &capture)
            .expect("capture revision");
        let second = library
            .capture_current_revision(wiki_id, collection_id, &capture)
            .expect("repeat capture");
        assert_eq!(first, second);
        assert_eq!(table_count(&library, "pages"), 1);
        assert_eq!(table_count(&library, "revisions"), 1);
        assert_eq!(table_count(&library, "collection_pages"), 1);
        assert_eq!(
            library
                .page(wiki_id, page_id)
                .expect("page query")
                .expect("stored page")
                .current_revision_id,
            Some(revision_id)
        );
        let revision = library
            .revision(wiki_id, revision_id)
            .expect("revision query")
            .expect("stored revision");
        assert_eq!(revision.content_object_id, first.id);
        assert_eq!(
            library
                .read_object(revision.content_object_id)
                .expect("canonical source"),
            capture.source
        );
    }

    #[test]
    fn integrity_metadata_scan_pages_media_subjects_in_stable_bounded_order() {
        let (_directory, library, media_object_id) = integrity_media_fixture();
        assert_eq!(library.integrity_metadata_record_count().expect("count"), 4);
        assert!(matches!(
            library.integrity_metadata_records_after(None, 0),
            Err(StoreError::InvalidConfig(_))
        ));
        assert!(matches!(
            library.integrity_metadata_records_after(None, MAX_INTEGRITY_METADATA_PAGE_SIZE + 1),
            Err(StoreError::InvalidConfig(_))
        ));

        let mut cursor = None;
        let mut records = Vec::new();
        loop {
            let page = library
                .integrity_metadata_records_after(cursor, 1)
                .expect("bounded metadata page");
            let Some(record) = page.into_iter().next() else {
                break;
            };
            cursor = Some(record.cursor().expect("record cursor"));
            records.push(record);
        }

        assert_eq!(records.len(), 4);
        assert!(records.iter().all(|record| record.issues.is_empty()));
        assert!(matches!(
            records.as_slice(),
            [
                IntegrityMetadataRecord {
                    subject: IntegrityMetadataSubject::Revision { .. },
                    ..
                },
                IntegrityMetadataRecord {
                    subject: IntegrityMetadataSubject::Page { .. },
                    ..
                },
                IntegrityMetadataRecord {
                    subject: IntegrityMetadataSubject::Media {
                        wiki_id: 1,
                        source_media_id: 9001,
                        ..
                    },
                    media_object: Some(IntegrityMediaObject {
                        object_id,
                        mime_type,
                        ..
                    }),
                    ..
                },
                IntegrityMetadataRecord {
                    subject: IntegrityMetadataSubject::PageMedia {
                        wiki_id: 1,
                        revision_id: 410,
                        placement_index: 0,
                        ..
                    },
                    ..
                }
            ] if *object_id == media_object_id && mime_type == "image/png"
        ));
    }

    #[test]
    fn integrity_metadata_scan_classifies_corrupt_media_references() {
        let (_directory, library, media_object_id) = integrity_media_fixture();
        library
            .connection()
            .pragma_update(None, "foreign_keys", false)
            .expect("disable fixture foreign keys");
        library
            .connection()
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("disable fixture check constraints");
        library
            .connection()
            .execute(
                "UPDATE content_objects SET object_kind = 'wikitext' WHERE object_id = ?1",
                [media_object_id.to_string()],
            )
            .expect("break media object kind");
        library
            .connection()
            .execute(
                "UPDATE media SET width = 0 WHERE source_media_id = 9001",
                [],
            )
            .expect("break media dimensions");
        library
            .connection()
            .execute(
                "UPDATE revisions SET page_id = 999 WHERE revision_id = 410",
                [],
            )
            .expect("remove placement page ownership");
        library
            .connection()
            .execute(
                "UPDATE page_media SET source_media_id = 9999,
                                       placement_kind = 'unexpected'",
                [],
            )
            .expect("break placement metadata and media pointer");
        library
            .connection()
            .execute(
                "INSERT INTO page_media (
                    wiki_id, revision_id, placement_index, source_media_id,
                    source_sha1, content_object_id, placement_kind, caption, alt_text
                 ) SELECT wiki_id, 999, 1, source_media_id, source_sha1,
                          content_object_id, 'inline', caption, alt_text
                   FROM page_media WHERE revision_id = 410",
                [],
            )
            .expect("insert missing-revision placement");

        let records = library
            .integrity_metadata_records_after(None, 100)
            .expect("corrupt metadata scan");
        let media = records
            .iter()
            .find(|record| matches!(record.subject, IntegrityMetadataSubject::Media { .. }))
            .expect("media record");
        assert!(
            media
                .issues
                .contains(&IntegrityMetadataIssue::MediaObjectWrongKind)
        );
        assert!(
            media
                .issues
                .contains(&IntegrityMetadataIssue::MediaMetadataInvalid)
        );
        assert_eq!(
            media.media_object.as_ref().map(|media| media.object_id),
            Some(media_object_id)
        );

        let placement_issues = records
            .iter()
            .filter_map(|record| match record.subject {
                IntegrityMetadataSubject::PageMedia { .. } => Some(&record.issues),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        assert!(placement_issues.contains(&IntegrityMetadataIssue::PageMediaMetadataInvalid));
        assert!(placement_issues.contains(&IntegrityMetadataIssue::PageMediaRevisionMissing));
        assert!(placement_issues.contains(&IntegrityMetadataIssue::PageMediaPageMissing));
        assert!(placement_issues.contains(&IntegrityMetadataIssue::PageMediaMediaMissing));
    }

    #[test]
    fn thumbnail_policy_is_durable_generation_tracked_and_defaults_off() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([PageTitle::new("Media policy page").expect("policy title")])
                .expect("policy selection"),
        );
        let collection_id = library
            .create_collection(
                wiki_id,
                "Media policy fixture",
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited(),
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect("create collection");
        let initial = library
            .collection_configuration(collection_id)
            .expect("read initial configuration")
            .expect("configured collection");
        assert_eq!(initial.image_policy, ImagePolicy::None);
        let initial_hash =
            manifest_configuration_hash_for(library.connection(), wiki_id, Some(collection_id))
                .expect("initial configuration hash");

        let thumbnails =
            ThumbnailPolicy::new(800, 12, 2 * 1024 * 1024).expect("bounded thumbnail policy");
        library
            .set_collection_image_policy(collection_id, ImagePolicy::Thumbnails(thumbnails))
            .expect("enable thumbnails");
        let configured = library
            .collection_configuration(collection_id)
            .expect("read thumbnail configuration")
            .expect("configured collection");
        assert_eq!(configured.image_policy, ImagePolicy::Thumbnails(thumbnails));
        assert_eq!(configured.generation, initial.generation + 1);
        assert_ne!(
            manifest_configuration_hash_for(library.connection(), wiki_id, Some(collection_id),)
                .expect("thumbnail configuration hash"),
            initial_hash
        );

        library
            .set_collection_image_policy(collection_id, ImagePolicy::None)
            .expect("disable thumbnails");
        assert_eq!(
            library
                .collection_configuration(collection_id)
                .expect("read disabled policy")
                .expect("configured collection")
                .image_policy,
            ImagePolicy::None
        );
        library
            .tombstone_collection(collection_id)
            .expect("tombstone collection");
        assert!(matches!(
            library.set_collection_image_policy(
                collection_id,
                ImagePolicy::Thumbnails(thumbnails)
            ),
            Err(StoreError::CollectionTombstoned(id)) if id == collection_id
        ));
    }

    #[test]
    fn thumbnail_capture_round_trips_complete_attribution_and_survives_tombstone() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Media fixture")
            .expect("create collection");
        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            41,
            410,
            "2026-08-22T10:00:00Z",
            "Media fixture article",
        );
        let page_id = PageId::new(41).expect("page ID");
        let revision_id = RevisionId::new(410).expect("revision ID");
        let file_title = PageTitle::new("File:Fixture.png").expect("file title");
        let bytes = VALID_PNG;
        let capture = ThumbnailCapture {
            media_id: MediaId::new(9001).expect("media ID"),
            file_title: &file_title,
            source_sha1: "abcdef0123456789abcdef0123456789",
            original_url: "https://upload.wikimedia.org/fixture.png",
            description_url: "https://commons.wikimedia.org/wiki/File:Fixture.png",
            author: "Fixture photographer",
            attribution: "Fixture photographer / Wikimedia Commons",
            license_name: "CC BY-SA 4.0",
            license_url: Some("https://creativecommons.org/licenses/by-sa/4.0/"),
            width: 1,
            height: 1,
            mime_type: ThumbnailMimeType::Png,
            captured_at: 1_776_000_000,
            source: bytes,
        };
        let placement = RevisionMediaPlacement {
            index: 0,
            kind: MediaPlacementKind::Lead,
            caption: Some("A complete fixture caption"),
            alt_text: Some("A descriptive fixture alternative"),
        };
        let policy = ThumbnailPolicy::new(640, 8, 1024).expect("thumbnail policy");

        let first = library
            .capture_revision_thumbnail(wiki_id, page_id, revision_id, policy, &capture, placement)
            .expect("capture thumbnail");
        let retry = ThumbnailCapture {
            captured_at: capture.captured_at + 60,
            ..capture.clone()
        };
        let second = library
            .capture_revision_thumbnail(wiki_id, page_id, revision_id, policy, &retry, placement)
            .expect("repeat thumbnail capture");
        assert_eq!(first, second);
        assert_eq!(first.kind, ObjectKind::Media);
        assert_eq!(
            library.read_object(first.id).expect("read thumbnail"),
            bytes
        );

        let stored = library
            .revision_media(wiki_id, revision_id)
            .expect("read revision media");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].placement_kind, MediaPlacementKind::Lead);
        assert_eq!(stored[0].caption.as_deref(), placement.caption);
        assert_eq!(stored[0].alt_text.as_deref(), placement.alt_text);
        assert_eq!(stored[0].media_id, capture.media_id);
        assert_eq!(stored[0].author, capture.author);
        assert_eq!(stored[0].attribution, capture.attribution);
        assert_eq!(stored[0].license_name, capture.license_name);
        assert_eq!(stored[0].license_url.as_deref(), capture.license_url);
        assert_eq!((stored[0].width, stored[0].height), (1, 1));
        assert_eq!(stored[0].mime_type, ThumbnailMimeType::Png);
        assert_eq!(stored[0].content_object_id, first.id);
        assert_eq!(stored[0].captured_at, capture.captured_at);

        let larger_rendition = ThumbnailCapture {
            original_url: "https://upload.wikimedia.org/fixture-2px.png",
            width: 2,
            captured_at: capture.captured_at + 120,
            source: SECOND_VALID_PNG,
            ..capture.clone()
        };
        let larger = library
            .capture_revision_thumbnail(
                wiki_id,
                page_id,
                revision_id,
                policy,
                &larger_rendition,
                RevisionMediaPlacement {
                    index: 1,
                    kind: MediaPlacementKind::Inline,
                    caption: Some("A second rendition"),
                    alt_text: placement.alt_text,
                },
            )
            .expect("capture second rendition of one source version");
        assert_ne!(larger.id, first.id);
        let renditions = library
            .revision_media(wiki_id, revision_id)
            .expect("read both renditions");
        assert_eq!(renditions.len(), 2);
        assert_eq!(renditions[0].content_object_id, first.id);
        assert_eq!(renditions[1].content_object_id, larger.id);
        assert_eq!((renditions[1].width, renditions[1].height), (2, 1));

        let conflicting_metadata = ThumbnailCapture {
            author: "Different author",
            ..capture.clone()
        };
        assert!(matches!(
            library.capture_revision_thumbnail(
                wiki_id,
                page_id,
                revision_id,
                policy,
                &conflicting_metadata,
                RevisionMediaPlacement {
                    index: 2,
                    kind: MediaPlacementKind::Inline,
                    caption: None,
                    alt_text: None,
                },
            ),
            Err(StoreError::ConflictingMedia(id)) if id == capture.media_id
        ));
        assert!(matches!(
            library.capture_revision_thumbnail(
                wiki_id,
                page_id,
                revision_id,
                policy,
                &capture,
                RevisionMediaPlacement {
                    caption: Some("Conflicting caption"),
                    ..placement
                },
            ),
            Err(StoreError::ConflictingMediaPlacement {
                revision_id: id,
                placement_index: 0,
            }) if id == revision_id
        ));
        assert_eq!(
            library
                .revision_media(wiki_id, revision_id)
                .expect("unchanged media after conflicts"),
            renditions
        );

        library
            .tombstone_collection(collection_id)
            .expect("stop tracking collection");
        assert_eq!(
            library
                .revision_media(wiki_id, revision_id)
                .expect("retained media after tombstone"),
            renditions
        );
        assert_eq!(
            library
                .read_object(first.id)
                .expect("retained thumbnail bytes"),
            bytes
        );
    }

    #[test]
    fn media_failure_never_changes_durable_text_or_existing_placement() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Media failure fixture")
            .expect("create collection");
        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            51,
            510,
            "2026-08-22T11:00:00Z",
            "Durable text before media",
        );
        let page_id = PageId::new(51).expect("page ID");
        let revision_id = RevisionId::new(510).expect("revision ID");
        let text_object = library
            .revision(wiki_id, revision_id)
            .expect("read revision")
            .expect("captured revision")
            .content_object_id;
        let object_count_before = library.logical_object_count().expect("object count");
        let file_title = PageTitle::new("File:Unsafe.svg").expect("file title");
        let invalid = ThumbnailCapture {
            media_id: MediaId::new(9002).expect("media ID"),
            file_title: &file_title,
            source_sha1: "1234567890abcdef1234567890abcdef",
            original_url: "https://upload.wikimedia.org/unsafe.svg",
            description_url: "https://commons.wikimedia.org/wiki/File:Unsafe.svg",
            author: "Fixture author",
            attribution: "Fixture attribution",
            license_name: "CC0",
            license_url: Some("https://creativecommons.org/publicdomain/zero/1.0/"),
            width: 100,
            height: 100,
            mime_type: ThumbnailMimeType::Png,
            captured_at: 1_776_000_001,
            source: b"<svg><script>unsafe()</script></svg>",
        };
        let policy = ThumbnailPolicy::new(640, 2, 1024).expect("thumbnail policy");
        assert!(matches!(
            library.capture_revision_thumbnail(
                wiki_id,
                page_id,
                revision_id,
                policy,
                &invalid,
                RevisionMediaPlacement {
                    index: 0,
                    kind: MediaPlacementKind::Inline,
                    caption: Some("Unsafe source"),
                    alt_text: None,
                },
            ),
            Err(StoreError::InvalidMediaMetadata(_))
        ));

        let malformed = b"\x89PNG\r\n\x1a\ntruncated".to_vec();
        let mut animated = VALID_PNG.to_vec();
        let animation_offset = animated.len() - 12;
        let mut animation_chunk = Vec::new();
        animation_chunk.extend_from_slice(&8_u32.to_be_bytes());
        animation_chunk.extend_from_slice(b"acTL");
        animation_chunk.extend_from_slice(&1_u32.to_be_bytes());
        animation_chunk.extend_from_slice(&0_u32.to_be_bytes());
        animation_chunk.extend_from_slice(&0_u32.to_be_bytes());
        animated.splice(animation_offset..animation_offset, animation_chunk);
        let mut dimension_bomb = VALID_PNG.to_vec();
        dimension_bomb[16..20].copy_from_slice(&100_000_u32.to_be_bytes());
        dimension_bomb[20..24].copy_from_slice(&100_000_u32.to_be_bytes());

        for (source, width, height, mime_type) in [
            (malformed, 1, 1, ThumbnailMimeType::Png),
            (animated, 1, 1, ThumbnailMimeType::Png),
            (dimension_bomb, 1, 1, ThumbnailMimeType::Png),
            (VALID_PNG.to_vec(), 2, 1, ThumbnailMimeType::Png),
            (VALID_PNG.to_vec(), 1, 1, ThumbnailMimeType::Jpeg),
        ] {
            let rejected = ThumbnailCapture {
                media_id: MediaId::new(9003).expect("adversarial media ID"),
                file_title: &file_title,
                source_sha1: "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
                original_url: "https://upload.wikimedia.org/adversarial.png",
                description_url: "https://commons.wikimedia.org/wiki/File:Adversarial.png",
                author: "Fixture author",
                attribution: "Fixture attribution",
                license_name: "CC0",
                license_url: None,
                width,
                height,
                mime_type,
                captured_at: 1_776_000_002,
                source: &source,
            };
            assert!(matches!(
                library.capture_revision_thumbnail(
                    wiki_id,
                    page_id,
                    revision_id,
                    policy,
                    &rejected,
                    RevisionMediaPlacement {
                        index: 0,
                        kind: MediaPlacementKind::Inline,
                        caption: None,
                        alt_text: None,
                    },
                ),
                Err(StoreError::InvalidMediaMetadata(_))
            ));
        }
        assert_eq!(
            library
                .logical_object_count()
                .expect("unchanged object count"),
            object_count_before
        );
        assert!(
            library
                .revision_media(wiki_id, revision_id)
                .expect("unchanged placements")
                .is_empty()
        );
        assert_eq!(
            library
                .read_object(text_object)
                .expect("durable text remains"),
            b"Durable text before media"
        );
    }

    #[test]
    fn collection_page_listing_rejects_a_collection_from_another_wiki() {
        let (_directory, mut library) = test_library();
        let first_wiki = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("first wiki");
        let second_wiki = library
            .register_wiki("https://de.wikipedia.org/w/api.php", "de")
            .expect("second wiki");
        let collection = library
            .create_explicit_collection(first_wiki, "Fixture collection")
            .expect("collection");

        assert!(matches!(
            library.collection_pages(second_wiki, collection),
            Err(StoreError::CollectionWikiMismatch)
        ));
    }

    #[test]
    fn historical_revisions_are_listed_newest_first_without_moving_the_head() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Systems languages")
            .expect("create collection");
        let title = PageTitle::new("Rust (programming language)").expect("title");
        let page_id = PageId::new(25_357_340).expect("page ID");
        let head_id = RevisionId::new(1_300_000_001).expect("revision ID");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id,
                    namespace: 0,
                    title: &title,
                    revision_id: head_id,
                    parent_id: Some(RevisionId::new(1_300_000_000).expect("parent")),
                    timestamp: "2026-08-19T12:34:56Z",
                    author: Some("Fixture editor"),
                    author_id: Some(42),
                    comment: Some("Improve the history section"),
                    minor: true,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"New source",
                },
            )
            .expect("capture head");
        let older_id = RevisionId::new(1_300_000_000).expect("older revision");
        let older = RevisionCapture {
            revision_id: older_id,
            parent_id: None,
            timestamp: "2026-08-18T10:00:00Z",
            author: None,
            author_id: None,
            comment: Some("Initial text"),
            minor: false,
            upstream_sha1: None,
            content_model: "wikitext",
            source: b"Old source",
        };
        library
            .capture_revision(wiki_id, page_id, &older)
            .expect("capture history");
        library
            .capture_revision(wiki_id, page_id, &older)
            .expect("repeat history capture");

        let history = library
            .revisions_for_page(wiki_id, page_id)
            .expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].revision_id, head_id);
        assert_eq!(history[1].revision_id, older_id);
        assert_eq!(history[1].comment.as_deref(), Some("Initial text"));
        assert_eq!(
            library
                .page(wiki_id, page_id)
                .expect("page")
                .expect("captured page")
                .current_revision_id,
            Some(head_id)
        );
        assert_eq!(
            library
                .revisions_by_id(older_id)
                .expect("revision matches")
                .len(),
            1
        );
    }

    #[test]
    fn newest_revision_at_or_before_uses_an_inclusive_cutoff() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Historical cutoff")
            .expect("create collection");
        let page_id = PageId::new(10).expect("page ID");
        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            page_id.get(),
            102,
            "2026-08-19T12:00:00Z",
            "Cutoff page",
        );
        library
            .capture_revision(
                wiki_id,
                page_id,
                &RevisionCapture {
                    revision_id: RevisionId::new(101).expect("revision ID"),
                    parent_id: None,
                    timestamp: "2026-08-19T11:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"Older source",
                },
            )
            .expect("capture history");

        let selected = library
            .newest_revision_for_page_at_or_before(wiki_id, page_id, "2026-08-19T11:00:00Z")
            .expect("bounded historical query")
            .expect("revision at inclusive cutoff");
        assert_eq!(selected.revision_id.get(), 101);
        assert!(
            library
                .newest_revision_for_page_at_or_before(wiki_id, page_id, "2026-08-19T10:59:59Z",)
                .expect("query before local history")
                .is_none()
        );
    }

    #[test]
    fn newest_revision_at_or_before_isolates_wikis_and_pages() {
        let (_directory, mut library) = test_library();
        let first_wiki = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register first wiki");
        let second_wiki = library
            .register_wiki("https://de.wikipedia.org/w/api.php", "de")
            .expect("register second wiki");
        let first_collection = library
            .create_explicit_collection(first_wiki, "First wiki")
            .expect("first collection");
        let second_collection = library
            .create_explicit_collection(second_wiki, "Second wiki")
            .expect("second collection");
        capture_test_page(
            &mut library,
            first_wiki,
            first_collection,
            10,
            100,
            "2026-08-19T10:00:00Z",
            "Selected page",
        );
        capture_test_page(
            &mut library,
            first_wiki,
            first_collection,
            20,
            200,
            "2026-08-19T12:00:00Z",
            "Other page",
        );
        capture_test_page(
            &mut library,
            second_wiki,
            second_collection,
            10,
            300,
            "2026-08-19T13:00:00Z",
            "Other wiki page",
        );

        let selected = library
            .newest_revision_for_page_at_or_before(
                first_wiki,
                PageId::new(10).expect("page ID"),
                "2026-08-20T00:00:00Z",
            )
            .expect("bounded historical query")
            .expect("selected revision");
        assert_eq!(selected.revision_id.get(), 100);
    }

    #[test]
    fn newest_revision_at_or_before_breaks_equal_timestamps_by_revision_id() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Equal timestamps")
            .expect("create collection");
        let page_id = PageId::new(10).expect("page ID");
        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            page_id.get(),
            100,
            "2026-08-19T12:00:00Z",
            "Tied page",
        );
        library
            .capture_revision(
                wiki_id,
                page_id,
                &RevisionCapture {
                    revision_id: RevisionId::new(101).expect("revision ID"),
                    parent_id: Some(RevisionId::new(100).expect("parent ID")),
                    timestamp: "2026-08-19T12:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"Equal timestamp source",
                },
            )
            .expect("capture tied revision");

        let selected = library
            .newest_revision_for_page_at_or_before(wiki_id, page_id, "2026-08-19T12:00:00Z")
            .expect("bounded historical query")
            .expect("selected revision");
        assert_eq!(selected.revision_id.get(), 101);
    }

    #[test]
    fn newest_revision_at_or_before_rejects_noncanonical_cutoffs() {
        let (_directory, library) = test_library();
        let error = library
            .newest_revision_for_page_at_or_before(
                WikiId::new(1).expect("wiki ID"),
                PageId::new(1).expect("page ID"),
                "2026-08-19T14:00:00+02:00",
            )
            .expect_err("offset timestamp must be normalized before querying");
        assert!(matches!(error, StoreError::InvalidConfig(_)));
    }

    #[test]
    fn historical_capture_rejects_unknown_pages_before_writing_an_object() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let page_id = PageId::new(25_357_340).expect("page ID");
        let revision = RevisionCapture {
            revision_id: RevisionId::new(1_300_000_000).expect("revision"),
            parent_id: None,
            timestamp: "2026-08-18T10:00:00Z",
            author: None,
            author_id: None,
            comment: None,
            minor: false,
            upstream_sha1: None,
            content_model: "wikitext",
            source: b"Unattached source",
        };

        assert!(matches!(
            library.capture_revision(wiki_id, page_id, &revision),
            Err(StoreError::PageNotFound { .. })
        ));
        assert_eq!(table_count(&library, "content_objects"), 0);
    }

    #[test]
    fn interrupted_sync_resumes_jobs_before_advancing_checkpoint() {
        let (directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        library
            .set_sync_overlap(wiki_id, 60)
            .expect("configure overlap");
        let started = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 1_000)
            .expect("start run");
        assert!(!started.resumed);
        assert_eq!(started.status.window_start, 0);
        let run_id = started.status.run_id;
        let first = library
            .enqueue_sync_job(run_id, "revision:10", "capture-revision", Some("10"))
            .expect("first job");
        assert_eq!(
            library
                .enqueue_sync_job(run_id, "revision:10", "capture-revision", Some("10"))
                .expect("idempotent job")
                .job_id,
            first.job_id
        );
        library
            .enqueue_sync_job(run_id, "revision:11", "capture-revision", Some("11"))
            .expect("second job");

        let claimed = library
            .claim_next_sync_job(run_id)
            .expect("claim first")
            .expect("first queued job");
        library
            .complete_sync_job(claimed.job_id)
            .expect("complete first");
        let failed = library
            .claim_next_sync_job(run_id)
            .expect("claim second")
            .expect("second queued job");
        library
            .fail_sync_job(
                failed.job_id,
                "source-timeout",
                "fixture request timed out",
                true,
            )
            .expect("record retryable failure");
        assert!(matches!(
            library.complete_sync_run(run_id, Some("cursor-before-durable")),
            Err(StoreError::IncompleteSyncRun {
                incomplete_jobs: 1,
                ..
            })
        ));
        assert_eq!(
            library.sync_checkpoints().expect("checkpoint")[0].committed_through,
            0
        );

        drop(library);
        let mut reopened = Library::open(directory.path()).expect("reopen library");
        let resumed = reopened
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 2_000)
            .expect("resume run");
        assert!(resumed.resumed);
        assert_eq!(resumed.status.run_id, run_id);
        assert_eq!(resumed.status.checkpoint_candidate, 1_000);
        assert_eq!(resumed.status.queued_jobs, 1);
        assert_eq!(resumed.status.succeeded_jobs, 1);
        assert_eq!(
            resumed
                .status
                .latest_error
                .as_ref()
                .map(|error| error.message.as_str()),
            Some("fixture request timed out")
        );
        let retried = reopened
            .claim_next_sync_job(run_id)
            .expect("claim retry")
            .expect("retryable job");
        assert_eq!(retried.job_id, failed.job_id);
        assert_eq!(retried.attempt_count, 2);
        reopened
            .complete_sync_job(retried.job_id)
            .expect("complete retry");
        let completed = reopened
            .complete_sync_run(run_id, Some("durable-cursor"))
            .expect("finish run");
        assert_eq!(completed.state, SyncRunState::Succeeded);

        let checkpoint = reopened.sync_checkpoints().expect("checkpoints").remove(0);
        assert_eq!(checkpoint.committed_through, 1_000);
        assert_eq!(checkpoint.next_window_start(), 940);
        assert_eq!(
            checkpoint.recent_changes_cursor.as_deref(),
            Some("durable-cursor")
        );
        let next = reopened
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 1_200)
            .expect("next overlap run");
        assert_eq!(next.status.window_start, 940);
    }

    #[test]
    fn running_collection_reconciliations_are_bounded_and_oldest_first() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let first_collection = library
            .create_explicit_collection(wiki_id, "First")
            .expect("create first collection");
        let second_collection = library
            .create_explicit_collection(wiki_id, "Second")
            .expect("create second collection");
        let first = library
            .start_or_resume_sync_run(
                wiki_id,
                Some(first_collection),
                SyncRunKind::Reconciliation,
                100,
            )
            .expect("start first reconciliation");
        let second = library
            .start_or_resume_sync_run(
                wiki_id,
                Some(second_collection),
                SyncRunKind::Reconciliation,
                101,
            )
            .expect("start second reconciliation");
        library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 102)
            .expect("start source update");

        let running = library
            .running_collection_reconciliations(100)
            .expect("list running reconciliations");
        assert_eq!(
            running
                .iter()
                .map(|status| status.run_id)
                .collect::<Vec<_>>(),
            vec![first.status.run_id, second.status.run_id]
        );
        assert_eq!(
            library
                .running_collection_reconciliations(1)
                .expect("bounded list")[0]
                .run_id,
            first.status.run_id
        );
        assert!(matches!(
            library.running_collection_reconciliations(0),
            Err(StoreError::InvalidConfig(_))
        ));
    }

    #[test]
    fn sync_job_keys_cannot_be_reused_for_different_work() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start run")
            .status
            .run_id;
        library
            .enqueue_sync_job(run_id, "title:Rust", "capture-page", Some("Rust"))
            .expect("enqueue");
        assert!(matches!(
            library.enqueue_sync_job(run_id, "title:Rust", "capture-page", Some("Ferris")),
            Err(StoreError::ConflictingSyncJobKey(_))
        ));
    }

    #[test]
    fn collection_checkpoints_are_independent() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let first_collection = library
            .create_explicit_collection(wiki_id, "First")
            .expect("first collection");
        let second_collection = library
            .create_explicit_collection(wiki_id, "Second")
            .expect("second collection");

        let first = library
            .start_or_resume_sync_run(wiki_id, Some(first_collection), SyncRunKind::Update, 1_000)
            .expect("first run");
        assert!(matches!(
            library.start_or_resume_sync_run(
                wiki_id,
                Some(first_collection),
                SyncRunKind::Reconciliation,
                1_000,
            ),
            Err(StoreError::SyncScopeBusy { .. })
        ));
        library
            .complete_sync_run(first.status.run_id, Some("first-cursor"))
            .expect("complete first");
        let second = library
            .start_or_resume_sync_run(wiki_id, Some(second_collection), SyncRunKind::Update, 1_200)
            .expect("second run");
        assert_eq!(second.status.window_start, 0);

        let checkpoints = library.sync_checkpoints().expect("checkpoints");
        assert_eq!(checkpoints.len(), 2);
        assert_eq!(checkpoints[0].collection_id, Some(first_collection));
        assert_eq!(checkpoints[0].committed_through, 1_000);
        assert_eq!(checkpoints[1].collection_id, Some(second_collection));
        assert_eq!(checkpoints[1].committed_through, 0);
    }

    #[test]
    fn successful_sync_manifests_are_idempotent_canonical_and_predecessor_linked() {
        let (directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let first_run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start first")
            .status
            .run_id;
        library
            .complete_sync_run(first_run, None)
            .expect("complete first");

        let first = library
            .append_sync_manifest(first_run)
            .expect("append first");
        let repeated = library
            .append_sync_manifest(first_run)
            .expect("repeat first");
        assert_eq!(first, repeated);
        assert_eq!(first.manifest.sequence, 1);
        assert_eq!(first.manifest.predecessor, None);
        assert_eq!(library.manifest_count().expect("count"), 1);

        let second_run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 200)
            .expect("start second")
            .status
            .run_id;
        library
            .complete_sync_run(second_run, Some("fixture-cursor"))
            .expect("complete second");
        let second = library
            .append_sync_manifest(second_run)
            .expect("append second");
        assert_eq!(second.manifest.sequence, 2);
        assert_eq!(second.manifest.predecessor, Some(first.id));
        assert_eq!(
            library.manifests_after(Some(1), 1).expect("page"),
            vec![second]
        );

        let filename = directory.path().join("manifests/000000000001.json");
        let bytes = fs::read(&filename).expect("manifest bytes");
        assert!(!bytes.ends_with(b"\n"));
        assert_eq!(
            serde_json::to_vec(&serde_json::from_slice::<ManifestEnvelope>(&bytes).unwrap())
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn media_aware_manifest_is_deterministic_and_schema_v1_remains_readable() {
        let (directory, mut library, media_object_id) = integrity_media_fixture();
        let wiki_id = WikiId::new(1).expect("wiki ID");
        let collection_id = CollectionId::new(1).expect("collection ID");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("start run")
            .status
            .run_id;
        library
            .complete_sync_run(run_id, None)
            .expect("complete run");
        let stored = library
            .append_sync_manifest(run_id)
            .expect("append manifest");
        let snapshot = stored
            .manifest
            .media_snapshot
            .as_ref()
            .expect("schema-v2 media snapshot");
        assert_eq!(snapshot.inventory.len(), 1);
        assert_eq!(snapshot.placements.len(), 1);
        assert_eq!(snapshot.inventory[0].content_object_id, media_object_id);
        assert_eq!(snapshot.placements[0].content_object_id, media_object_id);
        assert_eq!(
            snapshot,
            &library
                .manifest_media_snapshot(wiki_id, Some(collection_id))
                .expect("reproduce deterministic snapshot")
        );

        let path = directory.path().join("manifests/000000000001.json");
        let bytes = fs::read(&path).expect("manifest bytes");
        let mut legacy: ManifestEnvelope = serde_json::from_slice(&bytes).expect("manifest JSON");
        legacy.body.schema_version = 1;
        legacy.body.media_inventory = None;
        legacy.body.media_placements = None;
        let canonical_body = serde_json::to_vec(&legacy.body).expect("legacy body");
        legacy.manifest_id = ManifestId::for_body(&canonical_body).to_string();
        fs::write(&path, serde_json::to_vec(&legacy).expect("legacy manifest"))
            .expect("install legacy fixture");
        let legacy = library.read_manifest(1).expect("read schema-v1 manifest");
        assert!(legacy.manifest.media_snapshot.is_none());
        assert_eq!(legacy.manifest.introduced_revisions.len(), 1);
        assert_eq!(legacy.manifest.page_heads.len(), 1);
    }

    #[test]
    fn collection_usage_counts_distinct_active_revision_media_objects_once() {
        let (_directory, mut library, _media_object_id) = integrity_media_fixture();
        let wiki_id = WikiId::new(1).expect("wiki ID");
        let collection_id = CollectionId::new(1).expect("collection ID");
        let file_title = PageTitle::new("File:Integrity.png").expect("file title");
        let capture = ThumbnailCapture {
            media_id: MediaId::new(9001).expect("media ID"),
            file_title: &file_title,
            source_sha1: "abcdef0123456789abcdef0123456789",
            original_url: "https://upload.wikimedia.org/integrity.png",
            description_url: "https://commons.wikimedia.org/wiki/File:Integrity.png",
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
        library
            .capture_revision_thumbnail(
                wiki_id,
                PageId::new(41).expect("page ID"),
                RevisionId::new(410).expect("revision ID"),
                ThumbnailPolicy::new(640, 8, 1024).expect("thumbnail policy"),
                &capture,
                RevisionMediaPlacement {
                    index: 1,
                    kind: MediaPlacementKind::Inline,
                    caption: Some("Same bytes, second placement"),
                    alt_text: None,
                },
            )
            .expect("link shared media object twice");

        let estimate = library
            .collection_estimate(collection_id)
            .expect("collection usage");
        assert_eq!(
            estimate.current_canonical_bytes,
            "Integrity media article".len() as u64 + VALID_PNG.len() as u64
        );
    }

    #[test]
    fn manifests_reject_unfinished_runs_and_repair_bounded_success_gaps() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let running = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start running")
            .status
            .run_id;
        assert!(matches!(
            library.append_sync_manifest(running),
            Err(StoreError::SyncRunNotSucceeded(id)) if id == running
        ));
        library
            .complete_sync_run(running, None)
            .expect("complete first");
        let second = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 200)
            .expect("start second")
            .status
            .run_id;
        library
            .complete_sync_run(second, None)
            .expect("complete second");

        assert!(matches!(
            library.append_sync_manifest(second),
            Err(StoreError::ManifestRunOutOfOrder {
                expected,
                requested
            }) if expected == running && requested == second
        ));

        assert_eq!(
            library.unmanifested_succeeded_run_ids(10).expect("missing"),
            vec![running, second]
        );
        let repaired = library
            .append_missing_sync_manifests(1)
            .expect("repair one");
        assert_eq!(repaired.len(), 1);
        assert_eq!(repaired[0].manifest.run_id, running);
        assert_eq!(
            library
                .unmanifested_succeeded_run_ids(10)
                .expect("remaining"),
            vec![second]
        );
    }

    #[test]
    fn catalog_difference_introduces_preexisting_revisions_exactly_once() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Fixture")
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
                    parent_id: None,
                    timestamp: "2026-08-21T00:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"fixture",
                },
            )
            .expect("capture");
        let first_run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start first")
            .status
            .run_id;
        library
            .complete_sync_run(first_run, None)
            .expect("complete first");
        let first = library
            .append_sync_manifest(first_run)
            .expect("manifest first");
        assert_eq!(first.manifest.introduced_revisions.len(), 1);

        let second_run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 200)
            .expect("start second")
            .status
            .run_id;
        library
            .complete_sync_run(second_run, None)
            .expect("complete second");
        let second = library
            .append_sync_manifest(second_run)
            .expect("manifest second");
        assert!(second.manifest.introduced_revisions.is_empty());
    }

    #[test]
    fn manifest_repair_uses_configuration_hash_persisted_at_run_start() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Configuration fixture")
            .expect("collection");
        let started = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 100)
            .expect("start");
        let original_hash = started
            .status
            .configuration_hash
            .clone()
            .expect("snapshotted configuration");
        library
            .complete_sync_run(started.status.run_id, None)
            .expect("complete");

        library
            .connection
            .execute(
                "UPDATE collection_configuration
                 SET history_kind = 'complete', history_value = NULL
                 WHERE collection_id = ?1",
                [to_sql_integer(collection_id.get()).unwrap()],
            )
            .expect("mutate configuration after crash boundary");
        let changed_hash =
            manifest_configuration_hash_for(&library.connection, wiki_id, Some(collection_id))
                .expect("changed hash");
        assert_ne!(changed_hash, original_hash);

        let repaired = library
            .append_missing_sync_manifests(1)
            .expect("repair manifest");
        assert_eq!(repaired[0].manifest.configuration_hash, original_hash);
    }

    #[test]
    fn dump_import_resume_is_exact_monotonic_and_page_idempotent() {
        let (directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let title = PageTitle::new("Dump page").expect("title");
        let page_id = PageId::new(10).expect("page ID");
        let revision_id = RevisionId::new(100).expect("revision ID");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([title.clone()]).expect("selection"),
        );
        let collection_id = library
            .create_collection(
                wiki_id,
                "Dump fixture",
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited(),
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect("collection");
        let member = ResolvedCollectionMember {
            page_id,
            namespace: 0,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title.clone()),
        };
        library
            .commit_resolved_membership(collection_id, std::slice::from_ref(&member))
            .expect("resolve member");
        let generation = library
            .collection(collection_id)
            .expect("collection")
            .expect("present")
            .generation;
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 1_000)
            .expect("start bootstrap")
            .status
            .run_id;
        let digest = format!("b3:{}", "a".repeat(64));
        let request = DumpImportRequest {
            run_id,
            dump_digest: &digest,
            dump_compressed_bytes: 4_096,
            collection_generation: generation,
            bootstrap_started_at: 1_000,
        };
        let started = library
            .claim_or_resume_dump_import(request)
            .expect("claim import");
        assert!(!started.resumed);
        assert_eq!(started.status.state, DumpImportState::Running);
        assert_eq!(started.status.attempt_count, 1);
        let import_id = started.status.import_id;
        assert!(matches!(
            library.complete_sync_run(run_id, None),
            Err(StoreError::IncompleteSyncRun {
                run_id: incomplete,
                incomplete_jobs: 1,
            }) if incomplete == run_id
        ));

        let other_digest = format!("b3:{}", "b".repeat(64));
        assert!(matches!(
            library.claim_or_resume_dump_import(DumpImportRequest {
                dump_digest: &other_digest,
                ..request
            }),
            Err(StoreError::DumpImportIdentityMismatch {
                import_id: existing
            }) if existing == import_id
        ));

        capture_test_page(
            &mut library,
            wiki_id,
            collection_id,
            page_id.get(),
            revision_id.get(),
            "2026-08-23T10:00:00Z",
            title.as_str(),
        );
        let recorded = library
            .record_dump_imported_page(
                import_id,
                3,
                page_id,
                revision_id,
                title.as_str().len() as u64,
            )
            .expect("record imported page");
        assert_eq!(recorded.pages_scanned, 3);
        assert_eq!(recorded.imported_pages, 1);
        assert_eq!(
            recorded.imported_canonical_bytes,
            title.as_str().len() as u64
        );
        let repeated = library
            .record_dump_imported_page(
                import_id,
                4,
                page_id,
                revision_id,
                title.as_str().len() as u64,
            )
            .expect("repeat identical page");
        assert_eq!(repeated.pages_scanned, 4);
        assert_eq!(repeated.imported_pages, 1);
        assert!(matches!(
            library.record_dump_imported_page(
                import_id,
                4,
                page_id,
                revision_id,
                title.as_str().len() as u64 + 1,
            ),
            Err(StoreError::ConflictingDumpImportPage { page_id: conflict })
                if conflict == page_id
        ));
        assert!(matches!(
            library.record_dump_import_progress(import_id, 3),
            Err(StoreError::DumpImportProgressRegression { .. })
        ));
        library
            .fail_dump_import(import_id, "interrupted", "fixture interruption", true)
            .expect("fail retryably");
        assert!(matches!(
            library.record_dump_import_progress(import_id, 5),
            Err(StoreError::DumpImportNotRunning(id)) if id == import_id
        ));
        drop(library);

        let mut reopened = Library::open(directory.path()).expect("reopen");
        let resumed_run = reopened
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 2_000)
            .expect("resume bootstrap");
        assert!(resumed_run.resumed);
        let resumed = reopened
            .claim_or_resume_dump_import(request)
            .expect("resume import");
        assert!(resumed.resumed);
        assert_eq!(resumed.status.attempt_count, 2);
        assert_eq!(resumed.status.pages_scanned, 4);
        let repeated = reopened
            .record_dump_imported_page(
                import_id,
                5,
                page_id,
                revision_id,
                title.as_str().len() as u64,
            )
            .expect("repeat after restart");
        assert_eq!(repeated.pages_scanned, 5);
        assert_eq!(repeated.imported_pages, 1);
        let completed = reopened
            .complete_dump_import(import_id, 10)
            .expect("complete import");
        assert_eq!(completed.state, DumpImportState::Succeeded);
        assert_eq!(completed.pages_scanned, 10);
        reopened
            .complete_sync_run(run_id, None)
            .expect("complete owning sync run");
        assert!(matches!(
            reopened.record_dump_import_progress(import_id, 11),
            Err(StoreError::DumpImportNotRunning(id)) if id == import_id
        ));
        assert_eq!(
            reopened
                .dump_import_status(run_id)
                .expect("status")
                .expect("present"),
            completed
        );
    }

    #[test]
    fn dump_import_rejects_stale_selection_configuration_and_permanent_failure() {
        let (directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let title = PageTitle::new("Selected dump page").expect("title");
        let page_id = PageId::new(11).expect("page ID");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([title.clone()]).expect("selection"),
        );
        let collection_id = library
            .create_collection(
                wiki_id,
                "Stale dump fixture",
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited(),
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect("collection");
        let member = ResolvedCollectionMember {
            page_id,
            namespace: 0,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title),
        };
        library
            .commit_resolved_membership(collection_id, std::slice::from_ref(&member))
            .expect("resolve member");
        let generation = library
            .collection(collection_id)
            .expect("collection")
            .expect("present")
            .generation;
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 2_000)
            .expect("start bootstrap")
            .status
            .run_id;
        let digest = format!("b3:{}", "c".repeat(64));
        let request = DumpImportRequest {
            run_id,
            dump_digest: &digest,
            dump_compressed_bytes: 8_192,
            collection_generation: generation,
            bootstrap_started_at: 2_000,
        };
        let import_id = library
            .claim_or_resume_dump_import(request)
            .expect("claim")
            .status
            .import_id;
        library
            .commit_resolved_membership(collection_id, std::slice::from_ref(&member))
            .expect("advance generation");
        assert!(matches!(
            library.claim_or_resume_dump_import(request),
            Err(StoreError::StaleCollectionGeneration { .. })
        ));
        library
            .fail_dump_import(import_id, "bad-dump", "fixture permanent failure", false)
            .expect("fail permanently");
        drop(library);

        let mut reopened = Library::open(directory.path()).expect("reopen after permanent failure");
        let run = reopened
            .sync_run_status(run_id)
            .expect("run status")
            .expect("run exists");
        assert_eq!(run.state, SyncRunState::Cancelled);
        let import = reopened
            .dump_import_status(run_id)
            .expect("import status")
            .expect("import exists");
        assert_eq!(import.state, DumpImportState::Failed);
        assert!(!import.retryable);
        assert!(matches!(
            reopened.claim_or_resume_dump_import(request),
            Err(StoreError::SyncRunNotRunning(id)) if id == run_id
        ));
    }

    #[test]
    fn dump_import_does_not_adopt_bootstrap_jobs_without_an_import_identity() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Incompatible bootstrap")
            .expect("collection");
        let generation = library
            .collection(collection_id)
            .expect("collection")
            .expect("present")
            .generation;
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 4_000)
            .expect("start bootstrap")
            .status
            .run_id;
        library
            .enqueue_sync_job(run_id, "api-bootstrap:1", "capture-current", Some("1"))
            .expect("existing API bootstrap job");
        let digest = format!("b3:{}", "e".repeat(64));
        assert!(matches!(
            library.claim_or_resume_dump_import(DumpImportRequest {
                run_id,
                dump_digest: &digest,
                dump_compressed_bytes: 2_048,
                collection_generation: generation,
                bootstrap_started_at: 4_000,
            }),
            Err(StoreError::DumpImportRunHasExistingJobs {
                run_id: incompatible,
                jobs: 1,
            }) if incompatible == run_id
        ));
        assert!(
            library
                .dump_import_status(run_id)
                .expect("status query")
                .is_none()
        );
    }

    #[test]
    fn dump_import_rejects_configuration_change_without_generation_change() {
        let (_directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Transfer policy fixture")
            .expect("collection");
        let generation = library
            .collection(collection_id)
            .expect("collection")
            .expect("present")
            .generation;
        let run_id = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 3_000)
            .expect("start bootstrap")
            .status
            .run_id;
        library
            .update_network_transfer_policy(NetworkTransferPolicy::new(2, None, false).unwrap())
            .expect("change transfer policy");
        let digest = format!("b3:{}", "d".repeat(64));
        assert!(matches!(
            library.claim_or_resume_dump_import(DumpImportRequest {
                run_id,
                dump_digest: &digest,
                dump_compressed_bytes: 1_024,
                collection_generation: generation,
                bootstrap_started_at: 3_000,
            }),
            Err(StoreError::StaleDumpImportConfiguration { run_id: stale }) if stale == run_id
        ));
    }

    #[test]
    fn manifest_reads_detect_body_and_canonical_file_tampering() {
        let (directory, mut library) = test_library();
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start")
            .status
            .run_id;
        library.complete_sync_run(run_id, None).expect("complete");
        library.append_sync_manifest(run_id).expect("append");
        let path = directory.path().join("manifests/000000000001.json");
        let original = fs::read_to_string(&path).expect("manifest");
        let tampered = original.replace(
            "\"capture_completed_at\":100",
            "\"capture_completed_at\":101",
        );
        assert_ne!(tampered, original);
        fs::write(&path, tampered).expect("tamper body");
        assert!(matches!(
            library.read_manifest(1),
            Err(StoreError::CorruptManifest { sequence: 1, .. })
        ));

        fs::write(&path, format!("{original}\n")).expect("tamper representation");
        assert!(matches!(
            library.read_manifest(1),
            Err(StoreError::CorruptManifest { sequence: 1, .. })
        ));
    }

    fn migration_count(library: &Library) -> u32 {
        library
            .connection()
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("migration count")
    }

    fn table_count(library: &Library, table: &str) -> u32 {
        let statement = format!("SELECT COUNT(*) FROM {table}");
        library
            .connection()
            .query_row(&statement, [], |row| row.get(0))
            .expect("table count")
    }
}
