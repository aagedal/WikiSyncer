//! Typed service boundary for authenticated current-dump bootstrap.

use std::fs::{self, DirBuilder};
use std::io::{Cursor, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::runtime::Builder;
use wikisync_core::{CollectionId, HistoryPolicy, WikiId};
use wikisync_mediawiki::{
    ClientConfig, DumpAcquisitionLimits, DumpDigest, DumpLimits, MediaWikiClient, TrustedDumpIndex,
};
use wikisync_store::{DumpImportState, Library};
use wikisync_sync::{DumpBootstrapReport, bootstrap_collection_from_verified_dump};

use crate::{
    DaemonError, MeteredNetworkState, Mutation, OperationError,
    SET_CURRENT_DUMP_BOOTSTRAP_EXTENSION, detect_metered_network,
};

const ENCODING_VERSION: u8 = 1;
const MAX_INDEX_URL_BYTES: usize = 4 * 1024;
const MAX_DATABASE_BYTES: usize = 64;
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024 * 1024;
const MAX_TOTAL_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024 * 1024;
const MAX_ARTIFACTS: usize = 256;
const MAX_ELAPSED: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_COMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024 * 1024;
const MAX_DECOMPRESSED_BYTES: u64 = 8 * 1024 * 1024 * 1024 * 1024;
const MAX_PAGES: u64 = 100_000_000;
const MAX_PAGE_XML_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;
const MAX_METADATA_FIELD_BYTES: usize = 64 * 1024;
const MAX_SITEINFO_BYTES: u64 = 4 * 1024 * 1024;
const MAX_NAMESPACES: usize = 1_024;
const DUMP_CACHE_COMPONENTS: [&str; 2] = ["cache", "dumps"];

/// A caller-authenticated request to populate one current-only collection from a dump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentDumpBootstrapRequest {
    collection_id: CollectionId,
    trusted_index: TrustedDumpIndex,
    acquisition_limits: DumpAcquisitionLimits,
    parser_limits: DumpLimits,
    expected_collection_generation: Option<u64>,
}

impl CurrentDumpBootstrapRequest {
    /// Creates a request with bounded service defaults.
    pub fn new(
        collection_id: CollectionId,
        trusted_index: TrustedDumpIndex,
    ) -> Result<Self, OperationError> {
        Self {
            collection_id,
            trusted_index,
            acquisition_limits: DumpAcquisitionLimits::default(),
            parser_limits: DumpLimits::default(),
            expected_collection_generation: None,
        }
        .validate()
    }

    /// Replaces transfer and streaming-parser ceilings without weakening service bounds.
    pub fn with_limits(
        mut self,
        acquisition_limits: DumpAcquisitionLimits,
        parser_limits: DumpLimits,
    ) -> Result<Self, OperationError> {
        self.acquisition_limits = acquisition_limits;
        self.parser_limits = parser_limits;
        self.validate()
    }

    /// Durable collection scope.
    #[must_use]
    pub const fn collection_id(&self) -> CollectionId {
        self.collection_id
    }

    /// Caller-retained authenticated index identity.
    #[must_use]
    pub const fn trusted_index(&self) -> &TrustedDumpIndex {
        &self.trusted_index
    }

    /// Bounded acquisition ceilings.
    #[must_use]
    pub const fn acquisition_limits(&self) -> DumpAcquisitionLimits {
        self.acquisition_limits
    }

    /// Bounded streaming parser ceilings.
    #[must_use]
    pub const fn parser_limits(&self) -> DumpLimits {
        self.parser_limits
    }

    /// Binds execution to the exact collection generation observed by a preview.
    #[must_use]
    pub const fn with_expected_collection_generation(mut self, generation: u64) -> Self {
        self.expected_collection_generation = Some(generation);
        self
    }

    /// Collection generation that execution must still observe.
    #[must_use]
    pub const fn expected_collection_generation(&self) -> Option<u64> {
        self.expected_collection_generation
    }

    fn validate(self) -> Result<Self, OperationError> {
        validate_request(&self)?;
        Ok(self)
    }
}

/// Network-free summary of source, scope, trust identity, and all effective limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentDumpBootstrapPreview {
    pub collection_id: CollectionId,
    pub wiki_id: WikiId,
    pub collection_generation: u64,
    pub source_api_endpoint: String,
    pub source_language_code: String,
    pub selected_pages: u64,
    pub index_url: String,
    pub index_digest: String,
    pub expected_database: String,
    pub acquisition_limits: DumpAcquisitionLimits,
    pub parser_limits: DumpLimits,
    pub max_concurrent_requests: u32,
    pub max_download_bytes_per_second: Option<u64>,
    pub avoid_metered_networks: bool,
    pub maximum_collection_pages: Option<u64>,
    pub maximum_collection_canonical_bytes: Option<u64>,
    /// Library-relative private cache directory; dump bytes are never unpacked here.
    pub cache_directory: String,
}

/// Successful authenticated acquisition, import, closure, and durable receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentDumpBootstrapOutcome {
    pub preview: CurrentDumpBootstrapPreview,
    pub run_id: u64,
    pub import_id: u64,
    pub import_state: DumpImportState,
    pub resumed: bool,
    pub pages_scanned: u64,
    pub pages_imported: usize,
    pub pages_reused: usize,
    pub pages_absent_from_dump: usize,
    pub closure_pages_checked: usize,
    pub closure_differing_heads: usize,
    pub closure_missing_pages: usize,
    pub closure_pages_captured_from_api: usize,
    pub checkpoint_committed_through: u64,
}

/// Validates and previews a dump bootstrap without opening a network connection.
pub fn preview_current_dump_bootstrap(
    library: &Library,
    request: &CurrentDumpBootstrapRequest,
) -> Result<CurrentDumpBootstrapPreview, OperationError> {
    validate_request(request)?;
    let configuration = library
        .collection_configuration(request.collection_id)
        .map_err(failed)?
        .ok_or_else(|| OperationError::failed("collection has no committed configuration"))?;
    if configuration.history_policy != HistoryPolicy::CurrentAndFuture {
        return Err(OperationError::failed(
            "current dump bootstrap requires the current-and-future history policy",
        ));
    }
    let wiki = library
        .wiki(configuration.wiki_id)
        .map_err(failed)?
        .ok_or_else(|| OperationError::failed("collection source is missing"))?;
    if request
        .expected_collection_generation
        .is_some_and(|expected| expected != configuration.generation)
    {
        return Err(OperationError::failed(
            "collection changed after the current dump bootstrap preview",
        ));
    }
    let selected_pages = u64::try_from(
        library
            .resolved_collection_members(request.collection_id)
            .map_err(failed)?
            .len(),
    )
    .map_err(|_| OperationError::failed("selected page count exceeds platform limits"))?;
    if selected_pages == 0 {
        return Err(OperationError::failed(
            "current dump bootstrap requires at least one resolved page",
        ));
    }
    if selected_pages > request.parser_limits.max_pages {
        return Err(OperationError::failed(
            "selected page count exceeds the requested dump page ceiling",
        ));
    }
    if configuration
        .budget
        .maximum_pages()
        .is_some_and(|limit| selected_pages > limit.get())
    {
        return Err(OperationError::failed(
            "selected page count exceeds the collection hard page budget",
        ));
    }
    let network_policy = library.network_transfer_policy().map_err(failed)?;
    Ok(CurrentDumpBootstrapPreview {
        collection_id: request.collection_id,
        wiki_id: configuration.wiki_id,
        collection_generation: configuration.generation,
        source_api_endpoint: wiki.api_endpoint,
        source_language_code: wiki.language_code,
        selected_pages,
        index_url: request.trusted_index.url().to_owned(),
        index_digest: request.trusted_index.digest_hex(),
        expected_database: request.trusted_index.expected_database().to_owned(),
        acquisition_limits: request.acquisition_limits,
        parser_limits: request.parser_limits,
        max_concurrent_requests: network_policy.max_concurrent_requests(),
        max_download_bytes_per_second: network_policy.max_download_bytes_per_second(),
        avoid_metered_networks: network_policy.avoid_metered_networks(),
        maximum_collection_pages: configuration
            .budget
            .maximum_pages()
            .map(|value| value.get()),
        maximum_collection_canonical_bytes: configuration
            .budget
            .maximum_bytes()
            .map(|value| value.get()),
        cache_directory: DUMP_CACHE_COMPONENTS.join("/"),
    })
}

/// Acquires, authenticates, and imports a current dump under caller-held writer ownership.
pub fn bootstrap_collection_from_current_dump_direct(
    library: &mut Library,
    request: &CurrentDumpBootstrapRequest,
) -> Result<CurrentDumpBootstrapOutcome, OperationError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(OperationError::failed(
            "synchronous dump bootstrap cannot run inside a Tokio runtime; use the async direct API",
        ));
    }
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| OperationError::failed(format!("cannot start dump runtime: {error}")))?;
    runtime.block_on(bootstrap_collection_from_current_dump_direct_async(
        library, request,
    ))
}

/// Async direct-writer variant for callers that already run inside a Tokio task.
///
/// The caller must retain exclusive writer ownership until the returned future
/// completes. Synchronous CLI and daemon code should use
/// [`bootstrap_collection_from_current_dump_direct`] instead.
pub async fn bootstrap_collection_from_current_dump_direct_async(
    library: &mut Library,
    request: &CurrentDumpBootstrapRequest,
) -> Result<CurrentDumpBootstrapOutcome, OperationError> {
    if request.expected_collection_generation.is_none() {
        return Err(OperationError::failed(
            "current dump bootstrap execution requires a preview-bound collection generation",
        ));
    }
    let preview = preview_current_dump_bootstrap(library, request)?;
    let network_policy = library.network_transfer_policy().map_err(failed)?;
    if network_policy.avoid_metered_networks()
        && detect_metered_network().state == MeteredNetworkState::Metered
    {
        return Err(OperationError::failed(
            "dump bootstrap is blocked by the library policy while the active network is metered",
        ));
    }
    let request_slots = usize::try_from(network_policy.max_concurrent_requests())
        .map_err(|_| OperationError::failed("network concurrency policy is too large"))?;
    let byte_rate = network_policy
        .max_download_bytes_per_second()
        .map(usize::try_from)
        .transpose()
        .map_err(|_| OperationError::failed("network byte-rate policy is too large"))?;
    let client_config = ClientConfig::new(&preview.source_api_endpoint, user_agent())
        .and_then(|config| config.with_max_concurrent_requests(request_slots))
        .and_then(|config| config.with_max_downloaded_response_bytes_per_second(byte_rate))
        .map_err(failed)?;
    let client = MediaWikiClient::new(client_config).map_err(failed)?;
    let cache = ensure_dump_cache(library.root())?;
    let verified = client
        .acquire_current_dump_set(&request.trusted_index, &cache, request.acquisition_limits)
        .await
        .map_err(failed)?;
    // `VerifiedDumpSet` is deliberately constructed only by authenticated acquisition
    // and passed intact across the synchronization trust boundary.
    let report = bootstrap_collection_from_verified_dump(
        &client,
        library,
        request.collection_id,
        &verified,
        request.parser_limits,
    )
    .await
    .map_err(failed)?;
    outcome(preview, report)
}

pub(crate) fn current_dump_bootstrap_mutation(
    request: &CurrentDumpBootstrapRequest,
) -> Result<Mutation, DaemonError> {
    let payload = encode_request(request)?;
    Ok(Mutation::Extension {
        name: SET_CURRENT_DUMP_BOOTSTRAP_EXTENSION.to_owned(),
        payload,
    })
}

pub(crate) fn decode_current_dump_bootstrap_request(
    bytes: &[u8],
) -> Result<CurrentDumpBootstrapRequest, OperationError> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u8()? != ENCODING_VERSION {
        return Err(OperationError::failed(
            "unsupported current-dump request encoding",
        ));
    }
    let collection_id = CollectionId::new(decoder.u64()?).map_err(failed)?;
    let url = decoder.string(MAX_INDEX_URL_BYTES)?;
    let digest = DumpDigest::from_hex(&decoder.string(64)?).map_err(failed)?;
    let database = decoder.string(MAX_DATABASE_BYTES)?;
    let trusted_index = TrustedDumpIndex::new(&url, digest, database).map_err(failed)?;
    let acquisition_limits = DumpAcquisitionLimits {
        max_index_bytes: decoder.usize()?,
        max_artifact_bytes: decoder.u64()?,
        max_total_artifact_bytes: decoder.u64()?,
        max_artifacts: decoder.usize()?,
        max_elapsed: Duration::from_secs(decoder.u64()?),
    };
    let parser_limits = DumpLimits {
        max_compressed_bytes: decoder.u64()?,
        max_decompressed_bytes: decoder.u64()?,
        max_pages: decoder.u64()?,
        max_page_xml_bytes: decoder.u64()?,
        max_text_bytes: decoder.usize()?,
        max_metadata_field_bytes: decoder.usize()?,
        max_siteinfo_bytes: decoder.u64()?,
        max_namespaces: decoder.usize()?,
    };
    let expected_collection_generation = match decoder.u8()? {
        0 => None,
        1 => Some(decoder.u64()?),
        _ => {
            return Err(OperationError::failed(
                "invalid expected collection generation encoding",
            ));
        }
    };
    decoder.finish()?;
    let request = CurrentDumpBootstrapRequest::new(collection_id, trusted_index)?
        .with_limits(acquisition_limits, parser_limits)?;
    Ok(match expected_collection_generation {
        Some(generation) => request.with_expected_collection_generation(generation),
        None => request,
    })
}

pub(crate) fn encode_current_dump_bootstrap_outcome(
    outcome: &CurrentDumpBootstrapOutcome,
) -> Result<Vec<u8>, OperationError> {
    let mut bytes = Vec::with_capacity(512);
    bytes.push(ENCODING_VERSION);
    encode_preview(&mut bytes, &outcome.preview)?;
    put_u64(&mut bytes, outcome.run_id);
    put_u64(&mut bytes, outcome.import_id);
    bytes.push(match outcome.import_state {
        DumpImportState::Running => 0,
        DumpImportState::Succeeded => 1,
        DumpImportState::Failed => 2,
    });
    bytes.push(u8::from(outcome.resumed));
    put_u64(&mut bytes, outcome.pages_scanned);
    for value in [
        outcome.pages_imported,
        outcome.pages_reused,
        outcome.pages_absent_from_dump,
        outcome.closure_pages_checked,
        outcome.closure_differing_heads,
        outcome.closure_missing_pages,
        outcome.closure_pages_captured_from_api,
    ] {
        put_u64(
            &mut bytes,
            u64::try_from(value).map_err(|_| OperationError::failed("receipt count overflowed"))?,
        );
    }
    put_u64(&mut bytes, outcome.checkpoint_committed_through);
    Ok(bytes)
}

pub(crate) fn decode_current_dump_bootstrap_outcome(
    bytes: &[u8],
) -> Result<CurrentDumpBootstrapOutcome, DaemonError> {
    decode_outcome(bytes).map_err(|_| DaemonError::Protocol("invalid current-dump receipt"))
}

fn encode_request(request: &CurrentDumpBootstrapRequest) -> Result<Vec<u8>, DaemonError> {
    validate_request(request)
        .map_err(|_| DaemonError::Protocol("invalid current-dump bootstrap request"))?;
    let mut bytes = Vec::with_capacity(256);
    bytes.push(ENCODING_VERSION);
    put_u64(&mut bytes, request.collection_id.get());
    put_string(&mut bytes, request.trusted_index.url(), MAX_INDEX_URL_BYTES)
        .map_err(|_| DaemonError::Protocol("invalid current-dump index URL"))?;
    put_string(&mut bytes, &request.trusted_index.digest_hex(), 64)
        .map_err(|_| DaemonError::Protocol("invalid current-dump index digest"))?;
    put_string(
        &mut bytes,
        request.trusted_index.expected_database(),
        MAX_DATABASE_BYTES,
    )
    .map_err(|_| DaemonError::Protocol("invalid current-dump database"))?;
    put_u64(
        &mut bytes,
        request.acquisition_limits.max_index_bytes as u64,
    );
    put_u64(&mut bytes, request.acquisition_limits.max_artifact_bytes);
    put_u64(
        &mut bytes,
        request.acquisition_limits.max_total_artifact_bytes,
    );
    put_u64(&mut bytes, request.acquisition_limits.max_artifacts as u64);
    put_u64(&mut bytes, request.acquisition_limits.max_elapsed.as_secs());
    put_u64(&mut bytes, request.parser_limits.max_compressed_bytes);
    put_u64(&mut bytes, request.parser_limits.max_decompressed_bytes);
    put_u64(&mut bytes, request.parser_limits.max_pages);
    put_u64(&mut bytes, request.parser_limits.max_page_xml_bytes);
    put_u64(&mut bytes, request.parser_limits.max_text_bytes as u64);
    put_u64(
        &mut bytes,
        request.parser_limits.max_metadata_field_bytes as u64,
    );
    put_u64(&mut bytes, request.parser_limits.max_siteinfo_bytes);
    put_u64(&mut bytes, request.parser_limits.max_namespaces as u64);
    match request.expected_collection_generation {
        Some(generation) => {
            bytes.push(1);
            put_u64(&mut bytes, generation);
        }
        None => bytes.push(0),
    }
    Ok(bytes)
}

fn encode_preview(
    bytes: &mut Vec<u8>,
    preview: &CurrentDumpBootstrapPreview,
) -> Result<(), OperationError> {
    put_u64(bytes, preview.collection_id.get());
    put_u64(bytes, preview.wiki_id.get());
    put_u64(bytes, preview.collection_generation);
    put_string(bytes, &preview.source_api_endpoint, MAX_INDEX_URL_BYTES)?;
    put_string(bytes, &preview.source_language_code, 64)?;
    put_u64(bytes, preview.selected_pages);
    put_string(bytes, &preview.index_url, MAX_INDEX_URL_BYTES)?;
    put_string(bytes, &preview.index_digest, 64)?;
    put_string(bytes, &preview.expected_database, MAX_DATABASE_BYTES)?;
    let request = CurrentDumpBootstrapRequest::new(
        preview.collection_id,
        TrustedDumpIndex::new(
            &preview.index_url,
            DumpDigest::from_hex(&preview.index_digest).map_err(failed)?,
            &preview.expected_database,
        )
        .map_err(failed)?,
    )?
    .with_limits(preview.acquisition_limits, preview.parser_limits)?;
    let encoded =
        encode_request(&request).map_err(|error| OperationError::failed(error.to_string()))?;
    put_u64(bytes, encoded.len() as u64);
    bytes.extend_from_slice(&encoded);
    put_u64(bytes, u64::from(preview.max_concurrent_requests));
    put_option_u64(bytes, preview.max_download_bytes_per_second);
    bytes.push(u8::from(preview.avoid_metered_networks));
    put_option_u64(bytes, preview.maximum_collection_pages);
    put_option_u64(bytes, preview.maximum_collection_canonical_bytes);
    put_string(bytes, &preview.cache_directory, 64)?;
    Ok(())
}

fn decode_outcome(bytes: &[u8]) -> Result<CurrentDumpBootstrapOutcome, OperationError> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u8()? != ENCODING_VERSION {
        return Err(OperationError::failed(
            "unsupported current-dump receipt encoding",
        ));
    }
    let collection_id = CollectionId::new(decoder.u64()?).map_err(failed)?;
    let wiki_id = WikiId::new(decoder.u64()?).map_err(failed)?;
    let collection_generation = decoder.u64()?;
    let source_api_endpoint = decoder.string(MAX_INDEX_URL_BYTES)?;
    let source_language_code = decoder.string(64)?;
    let selected_pages = decoder.u64()?;
    let index_url = decoder.string(MAX_INDEX_URL_BYTES)?;
    let index_digest = decoder.string(64)?;
    let expected_database = decoder.string(MAX_DATABASE_BYTES)?;
    let request_bytes = decoder.bytes_with_limit(512)?;
    let request = decode_current_dump_bootstrap_request(&request_bytes)?;
    if request.collection_id != collection_id
        || request.trusted_index.url() != index_url
        || request.trusted_index.digest_hex() != index_digest
        || request.trusted_index.expected_database() != expected_database
    {
        return Err(OperationError::failed(
            "receipt preview identity is inconsistent",
        ));
    }
    let preview = CurrentDumpBootstrapPreview {
        collection_id,
        wiki_id,
        collection_generation,
        source_api_endpoint,
        source_language_code,
        selected_pages,
        index_url,
        index_digest,
        expected_database,
        acquisition_limits: request.acquisition_limits,
        parser_limits: request.parser_limits,
        max_concurrent_requests: u32::try_from(decoder.u64()?).map_err(|_| {
            OperationError::failed("dump receipt concurrency exceeds platform limits")
        })?,
        max_download_bytes_per_second: decoder.option_u64()?,
        avoid_metered_networks: decoder.bool()?,
        maximum_collection_pages: decoder.option_u64()?,
        maximum_collection_canonical_bytes: decoder.option_u64()?,
        cache_directory: decoder.string(64)?,
    };
    let run_id = decoder.u64()?;
    let import_id = decoder.u64()?;
    let import_state = match decoder.u8()? {
        0 => DumpImportState::Running,
        1 => DumpImportState::Succeeded,
        2 => DumpImportState::Failed,
        _ => return Err(OperationError::failed("invalid dump import receipt state")),
    };
    let resumed = decoder.bool()?;
    let pages_scanned = decoder.u64()?;
    let mut count = || {
        usize::try_from(decoder.u64()?)
            .map_err(|_| OperationError::failed("dump receipt count exceeds platform limits"))
    };
    let pages_imported = count()?;
    let pages_reused = count()?;
    let pages_absent_from_dump = count()?;
    let closure_pages_checked = count()?;
    let closure_differing_heads = count()?;
    let closure_missing_pages = count()?;
    let closure_pages_captured_from_api = count()?;
    let checkpoint_committed_through = decoder.u64()?;
    decoder.finish()?;
    Ok(CurrentDumpBootstrapOutcome {
        preview,
        run_id,
        import_id,
        import_state,
        resumed,
        pages_scanned,
        pages_imported,
        pages_reused,
        pages_absent_from_dump,
        closure_pages_checked,
        closure_differing_heads,
        closure_missing_pages,
        closure_pages_captured_from_api,
        checkpoint_committed_through,
    })
}

fn outcome(
    preview: CurrentDumpBootstrapPreview,
    report: DumpBootstrapReport,
) -> Result<CurrentDumpBootstrapOutcome, OperationError> {
    if report.import.state != DumpImportState::Succeeded {
        return Err(OperationError::failed(
            "dump bootstrap returned without a successful durable import",
        ));
    }
    Ok(CurrentDumpBootstrapOutcome {
        preview,
        run_id: report.status.run_id,
        import_id: report.import.import_id,
        import_state: report.import.state,
        resumed: report.resumed,
        pages_scanned: report.import.pages_scanned,
        pages_imported: report.pages_imported,
        pages_reused: report.pages_reused,
        pages_absent_from_dump: report.pages_absent_from_dump,
        closure_pages_checked: report.closure.pages_checked,
        closure_differing_heads: report.closure.differing_heads,
        closure_missing_pages: report.closure.missing_pages,
        closure_pages_captured_from_api: report.closure.pages_captured_from_api,
        checkpoint_committed_through: report.status.checkpoint_candidate,
    })
}

fn validate_request(request: &CurrentDumpBootstrapRequest) -> Result<(), OperationError> {
    if request.trusted_index.url().len() > MAX_INDEX_URL_BYTES
        || request.trusted_index.expected_database().len() > MAX_DATABASE_BYTES
    {
        return Err(OperationError::failed(
            "dump trust identity exceeds service bounds",
        ));
    }
    let acquisition = request.acquisition_limits;
    if acquisition.max_index_bytes == 0
        || acquisition.max_index_bytes > MAX_INDEX_BYTES
        || acquisition.max_artifact_bytes == 0
        || acquisition.max_artifact_bytes > MAX_ARTIFACT_BYTES
        || acquisition.max_total_artifact_bytes == 0
        || acquisition.max_total_artifact_bytes > MAX_TOTAL_ARTIFACT_BYTES
        || acquisition.max_artifact_bytes > acquisition.max_total_artifact_bytes
        || acquisition.max_artifacts == 0
        || acquisition.max_artifacts > MAX_ARTIFACTS
        || acquisition.max_elapsed.is_zero()
        || acquisition.max_elapsed > MAX_ELAPSED
    {
        return Err(OperationError::failed(
            "dump acquisition limits are outside service bounds",
        ));
    }
    let parser = request.parser_limits;
    if parser.max_compressed_bytes == 0
        || parser.max_compressed_bytes > MAX_COMPRESSED_BYTES
        || parser.max_decompressed_bytes == 0
        || parser.max_decompressed_bytes > MAX_DECOMPRESSED_BYTES
        || parser.max_pages == 0
        || parser.max_pages > MAX_PAGES
        || parser.max_page_xml_bytes == 0
        || parser.max_page_xml_bytes > MAX_PAGE_XML_BYTES
        || parser.max_text_bytes == 0
        || parser.max_text_bytes > MAX_TEXT_BYTES
        || parser.max_text_bytes as u64 > parser.max_page_xml_bytes
        || parser.max_metadata_field_bytes == 0
        || parser.max_metadata_field_bytes > MAX_METADATA_FIELD_BYTES
        || parser.max_siteinfo_bytes == 0
        || parser.max_siteinfo_bytes > MAX_SITEINFO_BYTES
        || parser.max_namespaces == 0
        || parser.max_namespaces > MAX_NAMESPACES
    {
        return Err(OperationError::failed(
            "dump parser limits are outside service bounds",
        ));
    }
    Ok(())
}

fn ensure_dump_cache(library_root: &Path) -> Result<PathBuf, OperationError> {
    let mut current = library_root.to_path_buf();
    for component in DUMP_CACHE_COMPONENTS {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(OperationError::failed(
                    "library dump cache path is not a safe directory",
                ));
            }
            Ok(metadata) => {
                if metadata.permissions().mode() & 0o077 != 0 {
                    return Err(OperationError::failed(
                        "library dump cache directory permissions are not private",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                builder.mode(0o700);
                match builder.create(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(failed(error)),
                }
                let metadata = fs::symlink_metadata(&current).map_err(failed)?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(OperationError::failed(
                        "library dump cache path changed during creation",
                    ));
                }
            }
            Err(error) => return Err(failed(error)),
        }
    }
    current.canonicalize().map_err(failed)
}

fn put_string(bytes: &mut Vec<u8>, value: &str, limit: usize) -> Result<(), OperationError> {
    if value.is_empty() || value.len() > limit {
        return Err(OperationError::failed(
            "encoded dump field exceeds its bound",
        ));
    }
    put_u64(bytes, value.len() as u64);
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_option_u64(bytes: &mut Vec<u8>, value: Option<u64>) {
    bytes.push(u8::from(value.is_some()));
    if let Some(value) = value {
        put_u64(bytes, value);
    }
}

struct Decoder<'a> {
    cursor: Cursor<&'a [u8]>,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(bytes),
        }
    }

    fn u8(&mut self) -> Result<u8, OperationError> {
        let mut value = [0; 1];
        self.cursor.read_exact(&mut value).map_err(failed)?;
        Ok(value[0])
    }

    fn bool(&mut self) -> Result<bool, OperationError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(OperationError::failed("invalid encoded dump boolean")),
        }
    }

    fn u64(&mut self) -> Result<u64, OperationError> {
        let mut value = [0; 8];
        self.cursor.read_exact(&mut value).map_err(failed)?;
        Ok(u64::from_be_bytes(value))
    }

    fn usize(&mut self) -> Result<usize, OperationError> {
        usize::try_from(self.u64()?)
            .map_err(|_| OperationError::failed("encoded dump limit exceeds platform bounds"))
    }

    fn string(&mut self, limit: usize) -> Result<String, OperationError> {
        let bytes = self.bytes_with_limit(limit)?;
        String::from_utf8(bytes).map_err(failed)
    }

    fn bytes_with_limit(&mut self, limit: usize) -> Result<Vec<u8>, OperationError> {
        let length = self.usize()?;
        if length == 0 || length > limit {
            return Err(OperationError::failed(
                "encoded dump field exceeds its bound",
            ));
        }
        let mut bytes = vec![0; length];
        self.cursor.read_exact(&mut bytes).map_err(failed)?;
        Ok(bytes)
    }

    fn option_u64(&mut self) -> Result<Option<u64>, OperationError> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.u64().map(Some),
            _ => Err(OperationError::failed("invalid encoded dump option")),
        }
    }

    fn finish(&self) -> Result<(), OperationError> {
        if self.cursor.position() == self.cursor.get_ref().len() as u64 {
            Ok(())
        } else {
            Err(OperationError::failed(
                "encoded dump value has trailing bytes",
            ))
        }
    }
}

fn failed(error: impl std::fmt::Display) -> OperationError {
    OperationError::failed(error.to_string())
}

fn user_agent() -> String {
    format!(
        "WikiSyncer-daemon/{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::Compression;
    use bzip2::write::BzEncoder;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::thread;
    use wikisync_core::{
        CollectionBudget, CollectionRemovalPolicy, CollectionRule, InclusionReason, PageId,
        PageTitle, TitleSelection,
    };
    use wikisync_store::{CollectionPreviewCommit, ResolvedCollectionMember};

    use crate::{ApplicationHandler, Client, Daemon};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug)]
    struct TempLibrary(PathBuf);

    impl TempLibrary {
        fn new() -> Self {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_dir()
                .expect("current directory")
                .join(format!(".wsd-dump-{}-{unique}", std::process::id()));
            fs::create_dir(&path).expect("create temporary library");
            Library::open(&path).expect("initialize temporary library");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempLibrary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug)]
    struct DumpFixtureServer {
        api_endpoint: String,
        index_url: String,
        index_digest: DumpDigest,
        requests: Arc<AtomicUsize>,
        expected_requests: usize,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl DumpFixtureServer {
        fn start() -> Self {
            Self::start_with_closure_failures(0)
        }

        fn start_with_closure_failures(closure_failures: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
            let address = listener.local_addr().expect("fixture address");
            let api_endpoint = format!("http://{address}/w/api.php");
            let index_url = format!("http://{address}/index.json");
            let xml = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo><sitename>Fixture</sitename><dbname>enwiki</dbname>
    <base>{api_endpoint}</base><generator>MediaWiki fixture</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter" /></namespaces>
  </siteinfo>
  <page><title>Alpha</title><ns>0</ns><id>10</id><revision>
    <id>100</id><parentid>99</parentid><timestamp>2026-08-23T10:00:00Z</timestamp>
    <contributor><username>Fixture editor</username><id>42</id></contributor>
    <comment>dump head</comment><model>wikitext</model><format>text/x-wiki</format>
    <text bytes="5" xml:space="preserve">Alpha</text>
  </revision></page>
</mediawiki>"#
            );
            let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
            encoder.write_all(xml.as_bytes()).expect("compress XML");
            let artifact = encoder.finish().expect("finish bzip2 artifact");
            let index = format!(
                r#"{{"schema":"wikisync-current-dump-index-v1","database":"enwiki","generated_at":"2026-08-23T10:02:00Z","artifacts":[{{"kind":"pages-meta-current-multistream","path":"fixture.xml.bz2","bytes":{},"blake3":"{}"}}]}}"#,
                artifact.len(),
                blake3::hash(&artifact).to_hex()
            )
            .into_bytes();
            let index_digest =
                DumpDigest::from_hex(blake3::hash(&index).to_hex().as_str()).expect("index digest");
            let unchanged = br#"{
              "batchcomplete":true,"query":{"pages":[{
                "pageid":10,"ns":0,"title":"Alpha","revisions":[{
                  "revid":100,"parentid":99,"timestamp":"2026-08-23T10:00:00Z","size":5
                }]
              }]}}
            "#
            .to_vec();
            let mut responses = vec![
                (200, index.clone(), "application/json"),
                (200, artifact, "application/x-bzip2"),
            ];
            for _ in 0..closure_failures {
                responses.push((503, b"{}".to_vec(), "application/json"));
            }
            if closure_failures > 0 {
                // A restarted writer re-authenticates the index, then reuses the
                // already verified cached artifact before resuming durable closure.
                responses.push((200, index, "application/json"));
            }
            responses.push((200, unchanged, "application/json"));
            let expected_requests = responses.len();
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&requests);
            let thread = thread::spawn(move || {
                for (status, body, content_type) in responses {
                    let (mut stream, _) = listener.accept().expect("accept fixture request");
                    read_request(&mut stream);
                    observed.fetch_add(1, Ordering::Release);
                    write_response(&mut stream, status, &body, content_type);
                }
            });
            Self {
                api_endpoint,
                index_url,
                index_digest,
                requests,
                expected_requests,
                thread: Some(thread),
            }
        }

        fn request(&self, collection_id: CollectionId) -> CurrentDumpBootstrapRequest {
            CurrentDumpBootstrapRequest::new(
                collection_id,
                TrustedDumpIndex::new(&self.index_url, self.index_digest, "enwiki")
                    .expect("trust anchor"),
            )
            .expect("dump request")
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::Acquire)
        }

        fn finish(mut self) {
            self.thread
                .take()
                .expect("fixture thread")
                .join()
                .expect("fixture server did not panic");
            assert_eq!(self.request_count(), self.expected_requests);
        }
    }

    fn read_request(stream: &mut TcpStream) {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1_024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).expect("read fixture request");
            assert!(read > 0, "client closed before headers");
            bytes.extend_from_slice(&buffer[..read]);
            assert!(bytes.len() < 64 * 1_024, "fixture request too large");
        }
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &[u8], content_type: &str) {
        let reason = if status == 200 {
            "OK"
        } else {
            "Service Unavailable"
        };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write fixture headers");
        stream.write_all(body).expect("write fixture body");
    }

    fn configured_collection(library: &mut Library, endpoint: &str) -> CollectionId {
        let wiki_id = library.register_wiki(endpoint, "en").expect("source");
        let title = PageTitle::new("Alpha").expect("title");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([title.clone()]).expect("selection"),
        );
        let member = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page ID"),
            namespace: 0,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title),
        };
        library
            .create_collection_from_preview(
                wiki_id,
                "Dump fixture",
                CollectionPreviewCommit {
                    rule: &rule,
                    history_policy: HistoryPolicy::CurrentAndFuture,
                    budget: CollectionBudget::unlimited()
                        .with_maximum_pages(1)
                        .expect("page budget")
                        .with_maximum_bytes(5)
                        .expect("byte budget"),
                    removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                    members: &[member],
                    missing_titles: &[],
                    predicted_canonical_bytes: Some(5),
                },
            )
            .expect("collection")
            .0
    }

    #[test]
    fn direct_preview_is_network_free_and_execution_returns_a_durable_receipt() {
        let server = DumpFixtureServer::start();
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("library");
        let collection_id = configured_collection(&mut library, &server.api_endpoint);
        let request = server.request(collection_id);

        let preview = preview_current_dump_bootstrap(&library, &request).expect("preview");
        assert_eq!(server.request_count(), 0, "preview contacted the source");
        assert_eq!(preview.selected_pages, 1);
        assert_eq!(preview.maximum_collection_canonical_bytes, Some(5));
        assert_eq!(preview.index_digest, server.index_digest.to_hex());
        let stale = request
            .clone()
            .with_expected_collection_generation(preview.collection_generation + 1);
        let stale_error = preview_current_dump_bootstrap(&library, &stale)
            .expect_err("stale preview binding must fail before networking");
        assert!(stale_error.message().contains("changed after"));
        assert_eq!(server.request_count(), 0);

        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("existing caller runtime");
        let nested_error = runtime
            .block_on(async {
                bootstrap_collection_from_current_dump_direct(&mut library, &request)
            })
            .expect_err("sync wrapper must fail safely inside an existing runtime");
        assert!(nested_error.message().contains("async direct API"));
        assert!(
            runtime
                .block_on(bootstrap_collection_from_current_dump_direct_async(
                    &mut library,
                    &request,
                ))
                .is_err(),
            "execution must be bound to the previewed generation"
        );
        let request = request.with_expected_collection_generation(preview.collection_generation);
        let outcome = runtime
            .block_on(bootstrap_collection_from_current_dump_direct_async(
                &mut library,
                &request,
            ))
            .expect("async-context direct dump bootstrap");
        assert_eq!(outcome.import_state, DumpImportState::Succeeded);
        assert_eq!(outcome.pages_imported, 1);
        assert_eq!(outcome.closure_pages_checked, 1);
        assert!(outcome.checkpoint_committed_through > 0);
        assert_eq!(
            library
                .dump_import_status(outcome.run_id)
                .expect("dump status")
                .expect("dump import")
                .import_id,
            outcome.import_id
        );
        let cache = temporary.path().join("cache/dumps");
        assert!(cache.is_dir());
        assert_eq!(
            fs::metadata(cache)
                .expect("cache metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        server.finish();
    }

    #[test]
    fn daemon_extension_forwards_the_typed_request_and_receipt_without_protocol_changes() {
        let server = DumpFixtureServer::start();
        let temporary = TempLibrary::new();
        let collection_id = {
            let mut library = Library::open(temporary.path()).expect("library");
            configured_collection(&mut library, &server.api_endpoint)
        };
        let request = server.request(collection_id);
        let preview = {
            let library = Library::open_read_only(temporary.path()).expect("preview library");
            preview_current_dump_bootstrap(&library, &request).expect("preview")
        };
        let request = request.with_expected_collection_generation(preview.collection_generation);
        let handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let daemon = Daemon::bind(temporary.path(), handler).expect("daemon");
        let daemon_thread = thread::spawn(move || daemon.run());
        let client = Client::for_library(temporary.path()).expect("client");

        let outcome = client
            .bootstrap_collection_from_current_dump(&request)
            .expect("forwarded dump bootstrap");
        assert_eq!(outcome.preview.collection_id, collection_id);
        assert_eq!(outcome.preview.index_digest, server.index_digest.to_hex());
        assert_eq!(outcome.import_state, DumpImportState::Succeeded);
        assert_eq!(outcome.pages_imported, 1);
        let status = client.status().expect("daemon status");
        assert_eq!(status.completed_mutations, 1);
        assert!(
            status
                .detail
                .contains("authenticated current dump bootstrap")
        );

        client.shutdown().expect("shutdown");
        daemon_thread
            .join()
            .expect("join daemon")
            .expect("daemon run");
        let library = Library::open(temporary.path()).expect("reopen library");
        assert!(
            library
                .dump_import_status(outcome.run_id)
                .unwrap()
                .is_some()
        );
        server.finish();
    }

    #[test]
    fn daemon_restart_reuses_verified_cache_and_resumes_the_same_durable_import() {
        let server = DumpFixtureServer::start_with_closure_failures(4);
        let temporary = TempLibrary::new();
        let collection_id = {
            let mut library = Library::open(temporary.path()).expect("library");
            configured_collection(&mut library, &server.api_endpoint)
        };
        let unbound = server.request(collection_id);
        let preview = {
            let library = Library::open_read_only(temporary.path()).expect("preview library");
            preview_current_dump_bootstrap(&library, &unbound).expect("preview")
        };
        let request = unbound.with_expected_collection_generation(preview.collection_generation);

        let first_handler = ApplicationHandler::new(temporary.path()).expect("first handler");
        let first_daemon = Daemon::bind(temporary.path(), first_handler).expect("first daemon");
        let first_thread = thread::spawn(move || first_daemon.run());
        let first_client = Client::for_library(temporary.path()).expect("first client");
        first_client
            .bootstrap_collection_from_current_dump(&request)
            .expect_err("exhausted closure request must retain resumable work");
        assert_eq!(
            first_client
                .status()
                .expect("failed daemon status")
                .completed_mutations,
            0,
            "a partial import must not be counted as a completed mutation"
        );
        first_client.shutdown().expect("first shutdown");
        first_thread
            .join()
            .expect("join first daemon")
            .expect("first daemon run");

        let interrupted = {
            let library = Library::open(temporary.path()).expect("interrupted library");
            let run = library
                .sync_run_statuses(1)
                .expect("sync status")
                .pop()
                .expect("interrupted run");
            let import = library
                .dump_import_status(run.run_id)
                .expect("dump status")
                .expect("interrupted import");
            assert_eq!(import.state, DumpImportState::Failed);
            assert!(import.retryable);
            assert_eq!(import.pages_scanned, 1);
            assert_eq!(import.attempt_count, 1);
            (run.run_id, import.import_id)
        };

        let second_handler = ApplicationHandler::new(temporary.path()).expect("second handler");
        let second_daemon = Daemon::bind(temporary.path(), second_handler).expect("second daemon");
        let second_thread = thread::spawn(move || second_daemon.run());
        let second_client = Client::for_library(temporary.path()).expect("second client");
        let resumed = second_client
            .bootstrap_collection_from_current_dump(&request)
            .expect("resumed dump bootstrap");
        assert!(resumed.resumed);
        assert_eq!((resumed.run_id, resumed.import_id), interrupted);
        assert_eq!(resumed.pages_imported, 0);
        assert_eq!(resumed.import_state, DumpImportState::Succeeded);
        assert_eq!(
            Library::open_read_only(temporary.path())
                .expect("resumed library")
                .dump_import_status(resumed.run_id)
                .expect("resumed status")
                .expect("resumed import")
                .attempt_count,
            2
        );
        assert_eq!(
            second_client
                .status()
                .expect("successful daemon status")
                .completed_mutations,
            1
        );
        second_client.shutdown().expect("second shutdown");
        second_thread
            .join()
            .expect("join second daemon")
            .expect("second daemon run");
        server.finish();
    }

    #[test]
    fn request_and_receipt_encodings_are_bounded_and_reject_trailing_bytes() {
        let trust = TrustedDumpIndex::new(
            "https://dumps.wikimedia.org/enwiki/index.json",
            DumpDigest::from_hex(&"01".repeat(32)).expect("digest"),
            "enwiki",
        )
        .expect("trust");
        let request =
            CurrentDumpBootstrapRequest::new(CollectionId::new(7).expect("collection"), trust)
                .expect("request");
        let encoded = encode_request(&request).expect("encode request");
        assert_eq!(
            decode_current_dump_bootstrap_request(&encoded).expect("decode request"),
            request
        );
        let mut trailing = encoded;
        trailing.push(0);
        assert!(decode_current_dump_bootstrap_request(&trailing).is_err());

        let oversized = request
            .clone()
            .with_limits(
                DumpAcquisitionLimits {
                    max_index_bytes: MAX_INDEX_BYTES + 1,
                    ..DumpAcquisitionLimits::default()
                },
                DumpLimits::default(),
            )
            .expect_err("oversized request must fail before IPC or network");
        assert!(oversized.message().contains("outside service bounds"));
    }
}
