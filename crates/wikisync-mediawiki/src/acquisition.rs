//! Authenticated, bounded acquisition of current-page dump artifacts.
//!
//! Wikimedia's legacy checksum listings are not a strong authenticity boundary. This
//! module therefore starts from a caller-retained BLAKE3 digest of a small index. The
//! authenticated index transitively commits to the database identity, timestamp,
//! ordered artifact names, exact lengths, and BLAKE3 digests. Callers must obtain and
//! retain that index digest through a channel they trust; HTTPS alone is transport
//! protection and is deliberately not described as archive authentication.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{StatusCode, Url, redirect};
use serde::Deserialize;

use crate::{
    AllowedOrigin, ClientError, DestinationResolver, MediaWikiClient, WIKIMEDIA_PROJECT_DOMAINS,
    redirect_destination_allowed,
};

/// Schema identifier required in an authenticated WikiSyncer dump index.
pub const CURRENT_DUMP_INDEX_SCHEMA: &str = "wikisync-current-dump-index-v1";
/// Artifact kind accepted by the stable current-pages bootstrap.
pub const CURRENT_DUMP_ARTIFACT_KIND: &str = "pages-meta-current-multistream";
const WIKIMEDIA_DUMPS_HOST: &str = "dumps.wikimedia.org";
const MAX_DATABASE_BYTES: usize = 64;
const MAX_TIMESTAMP_BYTES: usize = 64;
const MAX_ARTIFACT_NAME_BYTES: usize = 200;
const COPY_BUFFER_BYTES: usize = 128 * 1024;

/// A strong BLAKE3 digest used as a dump-index trust anchor and artifact identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DumpDigest([u8; 32]);

impl DumpDigest {
    /// Stable algorithm label for manifests and durable metadata.
    pub const ALGORITHM: &'static str = "blake3-256";

    /// Parses exactly 64 lowercase or uppercase hexadecimal digits.
    pub fn from_hex(value: &str) -> Result<Self, DumpAcquisitionError> {
        if value.len() != 64 {
            return Err(DumpAcquisitionError::InvalidDigest);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = decode_hex(pair[0]).ok_or(DumpAcquisitionError::InvalidDigest)?;
            let low = decode_hex(pair[1]).ok_or(DumpAcquisitionError::InvalidDigest)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    /// Returns the canonical lowercase hexadecimal representation.
    #[must_use]
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    /// Raw 32-byte digest for binary durable formats.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn hash(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

fn decode_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Out-of-band trust material for one bounded WikiSyncer dump index.
///
/// The index is a strict JSON object with `schema`, `database`, `generated_at`, and
/// `artifacts`. Each artifact has `kind`, a same-directory single-component `path`,
/// exact `bytes`, and a 64-digit `blake3`. Unknown fields are rejected. A producer
/// may derive this envelope from Wikimedia's published dump inventory, but the
/// caller must authenticate and retain the digest of the complete envelope outside
/// the downloaded cache. An unauthenticated digest fetched beside the index is not a
/// trust anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedDumpIndex {
    url: Url,
    digest: DumpDigest,
    expected_database: String,
}

impl TrustedDumpIndex {
    /// Creates a trust anchor. The URL itself is still checked against the client's
    /// exact dump-origin policy before any request is made.
    pub fn new(
        url: &str,
        digest: DumpDigest,
        expected_database: impl Into<String>,
    ) -> Result<Self, DumpAcquisitionError> {
        let url = Url::parse(url).map_err(|_| DumpAcquisitionError::InvalidIndexUrl)?;
        if url.cannot_be_a_base()
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(DumpAcquisitionError::InvalidIndexUrl);
        }
        let expected_database = expected_database.into();
        validate_bounded_text(&expected_database, MAX_DATABASE_BYTES)
            .map_err(|()| DumpAcquisitionError::InvalidDatabase)?;
        Ok(Self {
            url,
            digest,
            expected_database,
        })
    }

    /// Authenticated index URL, without credentials, query, or fragment.
    #[must_use]
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    /// Caller-retained BLAKE3 digest authenticating the complete index bytes.
    #[must_use]
    pub const fn digest(&self) -> DumpDigest {
        self.digest
    }

    /// Stable digest algorithm label used by this artifact.
    #[must_use]
    pub const fn digest_algorithm(&self) -> &'static str {
        DumpDigest::ALGORITHM
    }

    /// Canonical lowercase digest text for manifests and checkpoints.
    #[must_use]
    pub fn digest_hex(&self) -> String {
        self.digest.to_hex()
    }

    /// Database identity the caller expects, such as `enwiki`.
    #[must_use]
    pub fn expected_database(&self) -> &str {
        &self.expected_database
    }
}

/// Resource ceilings for one complete authenticated acquisition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumpAcquisitionLimits {
    /// Maximum bytes in the authenticated JSON index.
    pub max_index_bytes: usize,
    /// Maximum compressed bytes in one dump artifact.
    pub max_artifact_bytes: u64,
    /// Maximum compressed bytes declared across all artifacts.
    pub max_total_artifact_bytes: u64,
    /// Maximum artifact entries in the index.
    pub max_artifacts: usize,
    /// Whole-operation wall-clock ceiling. Partial files remain resumable on timeout.
    pub max_elapsed: Duration,
}

impl Default for DumpAcquisitionLimits {
    fn default() -> Self {
        Self {
            max_index_bytes: 1024 * 1024,
            max_artifact_bytes: 512 * 1024 * 1024 * 1024,
            max_total_artifact_bytes: 2 * 1024 * 1024 * 1024 * 1024,
            max_artifacts: 256,
            max_elapsed: Duration::from_secs(24 * 60 * 60),
        }
    }
}

impl DumpAcquisitionLimits {
    fn validate(self) -> Result<Self, DumpAcquisitionError> {
        if self.max_index_bytes == 0 {
            return Err(DumpAcquisitionError::InvalidLimit("index bytes"));
        }
        if self.max_artifact_bytes == 0 {
            return Err(DumpAcquisitionError::InvalidLimit("artifact bytes"));
        }
        if self.max_total_artifact_bytes == 0 {
            return Err(DumpAcquisitionError::InvalidLimit("total artifact bytes"));
        }
        if self.max_artifacts == 0 {
            return Err(DumpAcquisitionError::InvalidLimit("artifact count"));
        }
        if self.max_elapsed.is_zero() {
            return Err(DumpAcquisitionError::InvalidLimit("elapsed time"));
        }
        if self.max_artifact_bytes > self.max_total_artifact_bytes {
            return Err(DumpAcquisitionError::InvalidLimit(
                "artifact bytes must not exceed total artifact bytes",
            ));
        }
        Ok(self)
    }
}

/// One durably cached artifact whose length and digest were authenticated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDumpArtifact {
    path: PathBuf,
    length: u64,
    digest: DumpDigest,
}

impl VerifiedDumpArtifact {
    /// Absolute path to the regular cached file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Exact authenticated compressed length.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }

    /// BLAKE3 identity committed by the trusted index.
    #[must_use]
    pub const fn digest(&self) -> DumpDigest {
        self.digest
    }

    /// Opens the artifact only if it is still a regular file whose bytes match the
    /// authenticated length and digest. The returned handle is rewound to byte zero.
    pub fn open(&self) -> Result<File, DumpAcquisitionError> {
        let mut file = open_read_no_follow(&self.path)?;
        let metadata = file.metadata().map_err(DumpAcquisitionError::Io)?;
        if !metadata.file_type().is_file() || metadata.len() != self.length {
            return Err(DumpAcquisitionError::CachedArtifactChanged);
        }
        let hasher = hash_prefix(&mut file, self.length)?;
        if DumpDigest(*hasher.finalize().as_bytes()) != self.digest {
            return Err(DumpAcquisitionError::CachedArtifactChanged);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(DumpAcquisitionError::Io)?;
        Ok(file)
    }
}

/// Ordered, authenticated dump artifacts and their trusted source identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDumpSet {
    database_name: String,
    generated_at: String,
    source_index_url: String,
    index_digest: DumpDigest,
    artifacts: Vec<VerifiedDumpArtifact>,
}

/// Authenticated metadata for an ordered current-page dump set.
///
/// Unlike [`VerifiedDumpSet`], an inventory does not imply that every artifact has
/// already been downloaded. Callers can acquire one artifact at a time and make its
/// pages available before transferring the remainder of a large edition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDumpInventory {
    database_name: String,
    generated_at: String,
    source_index_url: String,
    index_digest: DumpDigest,
    artifacts: Vec<ValidatedArtifact>,
}

impl VerifiedDumpInventory {
    /// Database identity committed by the authenticated index.
    #[must_use]
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// Source timestamp committed by the authenticated index.
    #[must_use]
    pub fn generated_at(&self) -> &str {
        &self.generated_at
    }

    /// Exact authenticated index URL.
    #[must_use]
    pub fn source_index_url(&self) -> &str {
        &self.source_index_url
    }

    /// BLAKE3 identity of the authenticated index bytes.
    #[must_use]
    pub const fn index_digest(&self) -> DumpDigest {
        self.index_digest
    }

    /// Number of ordered artifacts committed by the index.
    #[must_use]
    pub fn artifact_count(&self) -> usize {
        self.artifacts.len()
    }

    /// Exact compressed bytes committed across the complete artifact set.
    pub fn total_compressed_bytes(&self) -> Result<u64, DumpAcquisitionError> {
        self.artifacts.iter().try_fold(0_u64, |total, artifact| {
            total
                .checked_add(artifact.length)
                .ok_or(DumpAcquisitionError::TotalSizeExceeded { limit: u64::MAX })
        })
    }

    /// Exact authenticated compressed length of one ordered artifact.
    #[must_use]
    pub fn artifact_length(&self, index: usize) -> Option<u64> {
        self.artifacts.get(index).map(|artifact| artifact.length)
    }

    /// Wraps one acquired artifact with the complete authenticated set identity.
    ///
    /// This is primarily useful for bounded parsers that consume one dump part at a
    /// time while retaining the full index provenance.
    pub fn single_artifact_set(
        &self,
        index: usize,
        artifact: VerifiedDumpArtifact,
    ) -> Result<VerifiedDumpSet, DumpAcquisitionError> {
        let expected = self
            .artifacts
            .get(index)
            .ok_or(DumpAcquisitionError::InvalidArtifactIndex)?;
        if artifact.length != expected.length || artifact.digest != expected.digest {
            return Err(DumpAcquisitionError::CachedArtifactChanged);
        }
        Ok(VerifiedDumpSet {
            database_name: self.database_name.clone(),
            generated_at: self.generated_at.clone(),
            source_index_url: self.source_index_url.clone(),
            index_digest: self.index_digest,
            artifacts: vec![artifact],
        })
    }
}

impl VerifiedDumpSet {
    #[must_use]
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// Timestamp asserted by the authenticated index. It is provenance, not a local
    /// observation of server time.
    #[must_use]
    pub fn generated_at(&self) -> &str {
        &self.generated_at
    }

    #[must_use]
    pub fn source_index_url(&self) -> &str {
        &self.source_index_url
    }

    #[must_use]
    pub const fn index_digest(&self) -> DumpDigest {
        self.index_digest
    }

    #[must_use]
    pub fn artifacts(&self) -> &[VerifiedDumpArtifact] {
        &self.artifacts
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexDocument {
    schema: String,
    database: String,
    generated_at: String,
    artifacts: Vec<IndexArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexArtifact {
    kind: String,
    path: String,
    bytes: u64,
    blake3: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidatedArtifact {
    name: String,
    url: Url,
    length: u64,
    digest: DumpDigest,
}

impl MediaWikiClient {
    /// Downloads and authenticates only the bounded dump index.
    ///
    /// Artifact bytes are deliberately deferred so a synchronization coordinator can
    /// alternate one durable download with one streaming import.
    pub async fn acquire_current_dump_inventory(
        &self,
        trust: &TrustedDumpIndex,
        limits: DumpAcquisitionLimits,
    ) -> Result<VerifiedDumpInventory, DumpAcquisitionError> {
        let limits = limits.validate()?;
        let origins = dump_origins(&self.config.endpoint);
        if !redirect_destination_allowed(&trust.url, &origins) {
            return Err(DumpAcquisitionError::OriginRejected);
        }
        let http = self.dump_http_client(&origins)?;
        tokio::time::timeout(limits.max_elapsed, async {
            let index_bytes = self
                .download_index(&http, trust.url.clone(), limits.max_index_bytes)
                .await?;
            if DumpDigest::hash(&index_bytes) != trust.digest {
                return Err(DumpAcquisitionError::IndexDigestMismatch);
            }
            let index: IndexDocument = serde_json::from_slice(&index_bytes)
                .map_err(DumpAcquisitionError::InvalidIndexJson)?;
            let artifacts = validate_index(index, trust, &origins, limits)?;
            Ok(VerifiedDumpInventory {
                database_name: trust.expected_database.clone(),
                generated_at: index_generated_at(&index_bytes)?,
                source_index_url: trust.url.as_str().to_owned(),
                index_digest: trust.digest,
                artifacts,
            })
        })
        .await
        .map_err(|_| DumpAcquisitionError::ElapsedTimeExceeded)?
    }

    /// Acquires and authenticates one artifact from a previously verified inventory.
    pub async fn acquire_current_dump_artifact(
        &self,
        inventory: &VerifiedDumpInventory,
        index: usize,
        cache_directory: &Path,
        limits: DumpAcquisitionLimits,
    ) -> Result<VerifiedDumpArtifact, DumpAcquisitionError> {
        let limits = limits.validate()?;
        let artifact = inventory
            .artifacts
            .get(index)
            .ok_or(DumpAcquisitionError::InvalidArtifactIndex)?;
        let cache_directory = validate_cache_directory(cache_directory)?;
        let origins = dump_origins(&self.config.endpoint);
        if !redirect_destination_allowed(&artifact.url, &origins) {
            return Err(DumpAcquisitionError::OriginRejected);
        }
        let http = self.dump_http_client(&origins)?;
        tokio::time::timeout(
            limits.max_elapsed,
            self.acquire_artifact(&http, &cache_directory, artifact),
        )
        .await
        .map_err(|_| DumpAcquisitionError::ElapsedTimeExceeded)?
    }

    /// Acquires and authenticates every ordered part in a current-pages dump index.
    ///
    /// Existing verified final files are reused. Interrupted downloads remain in a
    /// deterministic `.wikisync.part` file and are resumed only when the server
    /// returns an exact `206 Content-Range` for the locally verified prefix length.
    pub async fn acquire_current_dump_set(
        &self,
        trust: &TrustedDumpIndex,
        cache_directory: &Path,
        limits: DumpAcquisitionLimits,
    ) -> Result<VerifiedDumpSet, DumpAcquisitionError> {
        let inventory = self.acquire_current_dump_inventory(trust, limits).await?;
        let mut verified = Vec::with_capacity(inventory.artifact_count());
        for index in 0..inventory.artifact_count() {
            verified.push(
                self.acquire_current_dump_artifact(&inventory, index, cache_directory, limits)
                    .await?,
            );
        }
        Ok(VerifiedDumpSet {
            database_name: inventory.database_name,
            generated_at: inventory.generated_at,
            source_index_url: inventory.source_index_url,
            index_digest: inventory.index_digest,
            artifacts: verified,
        })
    }

    fn dump_http_client(
        &self,
        origins: &[AllowedOrigin],
    ) -> Result<reqwest::Client, DumpAcquisitionError> {
        let source_hosts = origins.iter().map(|origin| origin.host.clone()).collect();
        let redirect_origins = origins.to_vec();
        let max_redirects = self.config.max_redirects;
        reqwest::Client::builder()
            .user_agent(&self.config.user_agent)
            .connect_timeout(self.config.connect_timeout)
            .no_proxy()
            .dns_resolver(std::sync::Arc::new(DestinationResolver::system(
                source_hosts,
                self.config.destination_policy,
            )))
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > max_redirects {
                    return attempt.error("MediaWiki dump redirect limit exceeded");
                }
                if !redirect_destination_allowed(attempt.url(), &redirect_origins) {
                    return attempt.error("MediaWiki dump redirect left the approved origin");
                }
                attempt.follow()
            }))
            .https_only(self.config.endpoint.scheme() == "https")
            .build()
            .map_err(DumpAcquisitionError::Transport)
    }

    async fn download_index(
        &self,
        http: &reqwest::Client,
        url: Url,
        limit: usize,
    ) -> Result<Vec<u8>, DumpAcquisitionError> {
        let _permit = self
            .transport_limits
            .request_slots
            .acquire()
            .await
            .expect("MediaWiki request semaphore is never closed");
        let mut response =
            timeout_request(self.config.request_timeout, http.get(url).send()).await?;
        if !response.status().is_success() {
            return Err(DumpAcquisitionError::HttpStatus(response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(DumpAcquisitionError::IndexTooLarge { limit });
        }
        let reserved = response
            .content_length()
            .map(usize::try_from)
            .transpose()
            .map_err(|_| DumpAcquisitionError::IndexTooLarge { limit })?
            .unwrap_or(limit);
        let mut budget = self
            .transport_limits
            .reserve_response_capacity(reserved)
            .map_err(DumpAcquisitionError::Client)?;
        let mut bytes = Vec::with_capacity(reserved.min(64 * 1024));
        while let Some(chunk) = timeout_chunk(self.config.request_timeout, &mut response).await? {
            budget
                .record_chunk(chunk.len())
                .map_err(DumpAcquisitionError::Client)?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(DumpAcquisitionError::IndexTooLarge { limit });
            }
            if let Some(limiter) = &self.transport_limits.download_rate_limiter {
                limiter.consume(chunk.len()).await;
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn acquire_artifact(
        &self,
        http: &reqwest::Client,
        cache_directory: &Path,
        artifact: &ValidatedArtifact,
    ) -> Result<VerifiedDumpArtifact, DumpAcquisitionError> {
        let final_path = cache_directory.join(&artifact.name);
        if final_path.exists() {
            verify_file(&final_path, artifact.length, artifact.digest)?;
            return Ok(VerifiedDumpArtifact {
                path: final_path,
                length: artifact.length,
                digest: artifact.digest,
            });
        }
        let part_path = cache_directory.join(format!(".{}.wikisync.part", artifact.name));
        reject_symlink_if_present(&part_path)?;
        let mut file = open_partial(&part_path)?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| DumpAcquisitionError::ConcurrentAcquisition)?;
        let existing = file.metadata().map_err(DumpAcquisitionError::Io)?.len();
        if existing > artifact.length {
            return Err(DumpAcquisitionError::PartialFileTooLarge);
        }
        let mut hasher = hash_prefix(&mut file, existing)?;
        if existing == artifact.length {
            return self.publish_completed_partial(
                cache_directory,
                part_path,
                final_path,
                file,
                artifact,
                hasher,
            );
        }

        file.seek(SeekFrom::End(0))
            .map_err(DumpAcquisitionError::Io)?;
        let remaining = artifact.length - existing;
        let mut request = http.get(artifact.url.clone());
        if existing > 0 {
            request = request.header(RANGE, format!("bytes={existing}-"));
        }
        let _permit = self
            .transport_limits
            .request_slots
            .acquire()
            .await
            .expect("MediaWiki request semaphore is never closed");
        let mut response = timeout_request(self.config.request_timeout, request.send()).await?;
        if existing == 0 && response.status() != StatusCode::OK {
            return Err(DumpAcquisitionError::HttpStatus(response.status()));
        }
        if existing > 0 {
            validate_resume_response(&response, existing, artifact.length)?;
        }
        if response
            .content_length()
            .is_some_and(|length| length != remaining)
        {
            return Err(DumpAcquisitionError::InvalidContentLength);
        }
        let reserved = usize::try_from(remaining)
            .map_err(|_| DumpAcquisitionError::ArtifactTooLargeForPlatform)?;
        let mut budget = self
            .transport_limits
            .reserve_response_capacity(reserved)
            .map_err(DumpAcquisitionError::Client)?;
        let mut written = existing;
        loop {
            let chunk = match timeout_chunk(self.config.request_timeout, &mut response).await {
                Ok(chunk) => chunk,
                Err(error) => {
                    let _ = file.sync_data();
                    return Err(error);
                }
            };
            let Some(chunk) = chunk else { break };
            budget
                .record_chunk(chunk.len())
                .map_err(DumpAcquisitionError::Client)?;
            written = written
                .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
                .ok_or(DumpAcquisitionError::ArtifactLengthExceeded)?;
            if written > artifact.length {
                return Err(DumpAcquisitionError::ArtifactLengthExceeded);
            }
            if let Some(limiter) = &self.transport_limits.download_rate_limiter {
                limiter.consume(chunk.len()).await;
            }
            file.write_all(&chunk).map_err(DumpAcquisitionError::Io)?;
            hasher.update(&chunk);
        }
        if written != artifact.length {
            file.sync_data().map_err(DumpAcquisitionError::Io)?;
            return Err(DumpAcquisitionError::IncompleteArtifact);
        }
        self.publish_completed_partial(
            cache_directory,
            part_path,
            final_path,
            file,
            artifact,
            hasher,
        )
    }

    fn publish_completed_partial(
        &self,
        cache_directory: &Path,
        part_path: PathBuf,
        final_path: PathBuf,
        mut file: File,
        artifact: &ValidatedArtifact,
        hasher: blake3::Hasher,
    ) -> Result<VerifiedDumpArtifact, DumpAcquisitionError> {
        if DumpDigest(*hasher.finalize().as_bytes()) != artifact.digest {
            file.set_len(0).map_err(DumpAcquisitionError::Io)?;
            file.sync_all().map_err(DumpAcquisitionError::Io)?;
            return Err(DumpAcquisitionError::ArtifactDigestMismatch);
        }
        file.flush().map_err(DumpAcquisitionError::Io)?;
        file.sync_all().map_err(DumpAcquisitionError::Io)?;
        reject_symlink_if_present(&final_path)?;
        match fs::hard_link(&part_path, &final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                verify_file(&final_path, artifact.length, artifact.digest)?;
            }
            Err(error) => return Err(DumpAcquisitionError::Io(error)),
        }
        fs::remove_file(&part_path).map_err(DumpAcquisitionError::Io)?;
        File::open(cache_directory)
            .and_then(|directory| directory.sync_all())
            .map_err(DumpAcquisitionError::Io)?;
        Ok(VerifiedDumpArtifact {
            path: final_path,
            length: artifact.length,
            digest: artifact.digest,
        })
    }
}

fn validate_index(
    index: IndexDocument,
    trust: &TrustedDumpIndex,
    origins: &[AllowedOrigin],
    limits: DumpAcquisitionLimits,
) -> Result<Vec<ValidatedArtifact>, DumpAcquisitionError> {
    if index.schema != CURRENT_DUMP_INDEX_SCHEMA {
        return Err(DumpAcquisitionError::UnsupportedIndexSchema);
    }
    validate_bounded_text(&index.database, MAX_DATABASE_BYTES)
        .map_err(|()| DumpAcquisitionError::InvalidDatabase)?;
    if index.database != trust.expected_database {
        return Err(DumpAcquisitionError::DatabaseMismatch);
    }
    validate_bounded_text(&index.generated_at, MAX_TIMESTAMP_BYTES)
        .map_err(|()| DumpAcquisitionError::InvalidTimestamp)?;
    if index.artifacts.is_empty() || index.artifacts.len() > limits.max_artifacts {
        return Err(DumpAcquisitionError::ArtifactCountExceeded {
            limit: limits.max_artifacts,
        });
    }
    let mut names = BTreeSet::new();
    let mut total = 0_u64;
    let mut artifacts = Vec::with_capacity(index.artifacts.len());
    for item in index.artifacts {
        if item.kind != CURRENT_DUMP_ARTIFACT_KIND {
            return Err(DumpAcquisitionError::InvalidArtifactKind);
        }
        validate_artifact_name(&item.path)?;
        if !names.insert(item.path.clone()) {
            return Err(DumpAcquisitionError::DuplicateArtifact);
        }
        if item.bytes == 0 || item.bytes > limits.max_artifact_bytes {
            return Err(DumpAcquisitionError::ArtifactSizeExceeded {
                limit: limits.max_artifact_bytes,
            });
        }
        total = total
            .checked_add(item.bytes)
            .ok_or(DumpAcquisitionError::TotalSizeExceeded {
                limit: limits.max_total_artifact_bytes,
            })?;
        if total > limits.max_total_artifact_bytes {
            return Err(DumpAcquisitionError::TotalSizeExceeded {
                limit: limits.max_total_artifact_bytes,
            });
        }
        let url = trust
            .url
            .join(&item.path)
            .map_err(|_| DumpAcquisitionError::InvalidArtifactPath)?;
        if !redirect_destination_allowed(&url, origins) || url.query().is_some() {
            return Err(DumpAcquisitionError::OriginRejected);
        }
        artifacts.push(ValidatedArtifact {
            name: item.path,
            url,
            length: item.bytes,
            digest: DumpDigest::from_hex(&item.blake3)?,
        });
    }
    Ok(artifacts)
}

fn index_generated_at(bytes: &[u8]) -> Result<String, DumpAcquisitionError> {
    let index: IndexDocument =
        serde_json::from_slice(bytes).map_err(DumpAcquisitionError::InvalidIndexJson)?;
    Ok(index.generated_at)
}

fn validate_bounded_text(value: &str, maximum: usize) -> Result<(), ()> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(())
    } else {
        Ok(())
    }
}

fn validate_artifact_name(name: &str) -> Result<(), DumpAcquisitionError> {
    if name.is_empty()
        || name.len() > MAX_ARTIFACT_NAME_BYTES
        || name.starts_with('.')
        || !name.ends_with(".bz2")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(DumpAcquisitionError::InvalidArtifactPath);
    }
    Ok(())
}

fn dump_origins(endpoint: &Url) -> Vec<AllowedOrigin> {
    let mut origins = vec![AllowedOrigin::from_url(endpoint)];
    let is_wikimedia = endpoint.host_str().is_some_and(|host| {
        let host = host.to_ascii_lowercase();
        WIKIMEDIA_PROJECT_DOMAINS.iter().any(|domain| {
            host == *domain
                || host
                    .strip_suffix(domain)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    });
    if endpoint.scheme() == "https" && endpoint.port_or_known_default() == Some(443) && is_wikimedia
    {
        origins.push(AllowedOrigin {
            scheme: "https".to_owned(),
            host: WIKIMEDIA_DUMPS_HOST.to_owned(),
            port: 443,
        });
    }
    origins
}

fn validate_cache_directory(path: &Path) -> Result<PathBuf, DumpAcquisitionError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(DumpAcquisitionError::UnsafeCacheDirectory);
    }
    let metadata = fs::symlink_metadata(path).map_err(DumpAcquisitionError::Io)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DumpAcquisitionError::UnsafeCacheDirectory);
    }
    path.canonicalize().map_err(DumpAcquisitionError::Io)
}

fn reject_symlink_if_present(path: &Path) -> Result<(), DumpAcquisitionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(DumpAcquisitionError::UnsafeCacheEntry)
        }
        Ok(metadata) if !metadata.is_file() => Err(DumpAcquisitionError::UnsafeCacheEntry),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DumpAcquisitionError::Io(error)),
    }
}

fn open_partial(path: &Path) -> Result<File, DumpAcquisitionError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options.open(path).map_err(DumpAcquisitionError::Io)
}

fn open_read_no_follow(path: &Path) -> Result<File, DumpAcquisitionError> {
    reject_symlink_if_present(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options.open(path).map_err(DumpAcquisitionError::Io)
}

fn hash_prefix(file: &mut File, length: u64) -> Result<blake3::Hasher, DumpAcquisitionError> {
    file.seek(SeekFrom::Start(0))
        .map_err(DumpAcquisitionError::Io)?;
    let mut hasher = blake3::Hasher::new();
    let mut remaining = length;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        let read = file
            .read(&mut buffer[..wanted])
            .map_err(DumpAcquisitionError::Io)?;
        if read == 0 {
            return Err(DumpAcquisitionError::CachedArtifactChanged);
        }
        hasher.update(&buffer[..read]);
        remaining -= u64::try_from(read).unwrap();
    }
    Ok(hasher)
}

fn verify_file(
    path: &Path,
    expected_length: u64,
    expected_digest: DumpDigest,
) -> Result<(), DumpAcquisitionError> {
    let mut file = open_read_no_follow(path)?;
    let metadata = file.metadata().map_err(DumpAcquisitionError::Io)?;
    if !metadata.is_file() || metadata.len() != expected_length {
        return Err(DumpAcquisitionError::CachedArtifactChanged);
    }
    let hasher = hash_prefix(&mut file, expected_length)?;
    if DumpDigest(*hasher.finalize().as_bytes()) != expected_digest {
        return Err(DumpAcquisitionError::CachedArtifactChanged);
    }
    Ok(())
}

fn validate_resume_response(
    response: &reqwest::Response,
    offset: u64,
    total: u64,
) -> Result<(), DumpAcquisitionError> {
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(DumpAcquisitionError::ResumeRejected);
    }
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or(DumpAcquisitionError::InvalidContentRange)?;
    let value = value
        .strip_prefix("bytes ")
        .ok_or(DumpAcquisitionError::InvalidContentRange)?;
    let (range, declared_total) = value
        .split_once('/')
        .ok_or(DumpAcquisitionError::InvalidContentRange)?;
    let (start, end) = range
        .split_once('-')
        .ok_or(DumpAcquisitionError::InvalidContentRange)?;
    if start.parse::<u64>().ok() != Some(offset)
        || end.parse::<u64>().ok() != total.checked_sub(1)
        || declared_total.parse::<u64>().ok() != Some(total)
    {
        return Err(DumpAcquisitionError::InvalidContentRange);
    }
    Ok(())
}

async fn timeout_request(
    timeout: Duration,
    request: impl std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
) -> Result<reqwest::Response, DumpAcquisitionError> {
    tokio::time::timeout(timeout, request)
        .await
        .map_err(|_| DumpAcquisitionError::RequestTimedOut)?
        .map_err(DumpAcquisitionError::Transport)
}

async fn timeout_chunk(
    timeout: Duration,
    response: &mut reqwest::Response,
) -> Result<Option<bytes::Bytes>, DumpAcquisitionError> {
    tokio::time::timeout(timeout, response.chunk())
        .await
        .map_err(|_| DumpAcquisitionError::RequestTimedOut)?
        .map_err(DumpAcquisitionError::Transport)
}

/// A redacted acquisition, authentication, path, or resource-bound failure.
#[derive(Debug)]
pub enum DumpAcquisitionError {
    InvalidDigest,
    InvalidIndexUrl,
    InvalidDatabase,
    InvalidTimestamp,
    InvalidLimit(&'static str),
    UnsafeCacheDirectory,
    UnsafeCacheEntry,
    OriginRejected,
    IndexTooLarge { limit: usize },
    IndexDigestMismatch,
    InvalidIndexJson(serde_json::Error),
    UnsupportedIndexSchema,
    DatabaseMismatch,
    ArtifactCountExceeded { limit: usize },
    InvalidArtifactIndex,
    InvalidArtifactKind,
    InvalidArtifactPath,
    DuplicateArtifact,
    ArtifactSizeExceeded { limit: u64 },
    TotalSizeExceeded { limit: u64 },
    ArtifactTooLargeForPlatform,
    PartialFileTooLarge,
    ResumeRejected,
    InvalidContentRange,
    InvalidContentLength,
    ArtifactLengthExceeded,
    IncompleteArtifact,
    ArtifactDigestMismatch,
    CachedArtifactChanged,
    ConcurrentAcquisition,
    RequestTimedOut,
    ElapsedTimeExceeded,
    HttpStatus(StatusCode),
    Transport(reqwest::Error),
    Client(ClientError),
    Io(io::Error),
}

impl fmt::Display for DumpAcquisitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDigest => {
                formatter.write_str("dump digest is not 32-byte hexadecimal BLAKE3")
            }
            Self::InvalidIndexUrl => {
                formatter.write_str("dump index URL is not a safe absolute URL")
            }
            Self::InvalidDatabase => formatter.write_str("dump database identity is invalid"),
            Self::InvalidTimestamp => formatter.write_str("dump index timestamp is invalid"),
            Self::InvalidLimit(name) => {
                write!(formatter, "dump acquisition {name} limit is invalid")
            }
            Self::UnsafeCacheDirectory => {
                formatter.write_str("dump cache directory is not a safe absolute directory")
            }
            Self::UnsafeCacheEntry => formatter.write_str("dump cache entry is not a regular file"),
            Self::OriginRejected => {
                formatter.write_str("dump URL is outside the approved source origin")
            }
            Self::IndexTooLarge { limit } => {
                write!(formatter, "dump index exceeded the {limit}-byte limit")
            }
            Self::IndexDigestMismatch => {
                formatter.write_str("dump index did not match its trusted BLAKE3 digest")
            }
            Self::InvalidIndexJson(_) => {
                formatter.write_str("authenticated dump index is not valid bounded JSON")
            }
            Self::UnsupportedIndexSchema => {
                formatter.write_str("authenticated dump index schema is unsupported")
            }
            Self::DatabaseMismatch => {
                formatter.write_str("authenticated dump index names a different database")
            }
            Self::ArtifactCountExceeded { limit } => {
                write!(formatter, "dump index exceeded the {limit}-artifact limit")
            }
            Self::InvalidArtifactIndex => {
                formatter.write_str("dump artifact index is outside the authenticated set")
            }
            Self::InvalidArtifactKind => {
                formatter.write_str("dump index contains a non-current-pages artifact")
            }
            Self::InvalidArtifactPath => {
                formatter.write_str("dump artifact name is not a safe bounded path component")
            }
            Self::DuplicateArtifact => {
                formatter.write_str("dump index contains a duplicate artifact")
            }
            Self::ArtifactSizeExceeded { limit } => {
                write!(formatter, "dump artifact exceeded the {limit}-byte limit")
            }
            Self::TotalSizeExceeded { limit } => {
                write!(formatter, "dump set exceeded the {limit}-byte limit")
            }
            Self::ArtifactTooLargeForPlatform => {
                formatter.write_str("dump artifact is too large for this platform")
            }
            Self::PartialFileTooLarge => {
                formatter.write_str("partial dump file exceeds its authenticated length")
            }
            Self::ResumeRejected => {
                formatter.write_str("dump server did not honor the safe range-resume request")
            }
            Self::InvalidContentRange => {
                formatter.write_str("dump server returned a mismatched content range")
            }
            Self::InvalidContentLength => {
                formatter.write_str("dump response length differs from the authenticated length")
            }
            Self::ArtifactLengthExceeded => {
                formatter.write_str("dump response exceeded the authenticated artifact length")
            }
            Self::IncompleteArtifact => {
                formatter.write_str("dump response ended before the authenticated artifact length")
            }
            Self::ArtifactDigestMismatch => {
                formatter.write_str("dump artifact did not match its authenticated BLAKE3 digest")
            }
            Self::CachedArtifactChanged => {
                formatter.write_str("cached dump artifact changed after authentication")
            }
            Self::ConcurrentAcquisition => {
                formatter.write_str("dump artifact is already being acquired by another writer")
            }
            Self::RequestTimedOut => {
                formatter.write_str("dump request or response stalled past its time limit")
            }
            Self::ElapsedTimeExceeded => {
                formatter.write_str("dump acquisition exceeded its whole-operation time limit")
            }
            Self::HttpStatus(status) => {
                write!(formatter, "dump server returned HTTP status {status}")
            }
            Self::Transport(_) => formatter.write_str("dump transport failed"),
            Self::Client(error) => error.fmt(formatter),
            Self::Io(_) => formatter.write_str("dump cache I/O failed"),
        }
    }
}

impl Error for DumpAcquisitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidIndexJson(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Client(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
