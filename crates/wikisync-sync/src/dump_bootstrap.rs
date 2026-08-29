//! Selection-aware current-page dump bootstrap and its Action API race closure.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use url::Url;
use wikisync_core::{
    CollectionId, CollectionRule, HistoryPolicy, MAIN_NAMESPACE, PageId, PageTitle, WikiId,
};
use wikisync_mediawiki::{
    ClientError, DumpAcquisitionError, DumpAcquisitionLimits, DumpError, DumpLimits, DumpPage,
    DumpReader, DumpSiteInfo, MediaWikiClient, PageHeadResolution, RecentChangeKind,
    RecentChangesContinuation, TrustedDumpIndex, VerifiedDumpInventory, VerifiedDumpSet,
};
use wikisync_search::SqliteSearchIndex;
use wikisync_store::{
    CurrentRevisionCapture, DumpImportRequest, DumpImportState, DumpImportStatus, Library,
    StoreError, StoredWholeEditionChange, SyncRunKind, SyncRunStatus, WholeEditionChange,
    WholeEditionChangeDisposition, WholeEditionChangeKind, WholeEditionDiscoveryKind,
    WholeEditionDiscoveryRequest, WholeEditionDiscoveryStatus, WholeEditionImportRequest,
    WholeEditionImportStatus,
};

use super::{
    CaptureError, CapturedPage, MediaCaptureReport, NeverCancelled, PageReconciliationReport,
    ReconciliationLimits, capture_resolved_page_head, capture_revision_media,
    enforce_collection_byte_budget, index_stored_current_revision,
    index_stored_current_revision_with, mediawiki_timestamp_seconds, reconcile_page_head,
    repair_missing_sync_manifests, revision_capture, validate_content,
};

const WHOLE_EDITION_CHANGE_PAGE_SIZE: u32 = 500;
const MAX_SAFE_RECENT_CHANGES_WINDOW_SECONDS: u64 = 7 * 24 * 60 * 60;

/// API work performed after the streaming scan to close its race window.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DumpClosureReport {
    /// Selected stable page IDs queried after the dump scan.
    pub pages_checked: usize,
    /// Pages whose Action API head differed from the imported durable head.
    pub differing_heads: usize,
    /// Selected stable IDs unavailable from both the dump and Action API.
    pub missing_pages: usize,
    /// Selected pages absent from the dump but captured from the Action API.
    pub pages_captured_from_api: usize,
    /// Bounded revision-list responses used to connect imported heads to new heads.
    pub revision_batches: usize,
    /// Intermediate revisions enumerated while closing the window.
    pub revisions_enumerated: usize,
    /// Intermediate canonical revisions newly captured.
    pub revisions_captured: usize,
    /// Already durable intermediate revisions reused.
    pub revisions_reused: usize,
    /// Optional media work performed by the common reconciliation path.
    pub media: MediaCaptureReport,
}

/// Completed current-dump bootstrap, including its final durable boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpBootstrapReport {
    /// Completed Bootstrap run whose checkpoint is the pre-scan timestamp.
    pub status: SyncRunStatus,
    /// Durable dump-import status bound to the authenticated artifact.
    pub import: DumpImportStatus,
    /// Whether this invocation resumed an existing scan.
    pub resumed: bool,
    /// Selected dump pages attached during this invocation.
    pub pages_imported: usize,
    /// Selected dump revisions already durable when encountered.
    pub pages_reused: usize,
    /// Selected stable IDs not present in the filtered dump.
    pub pages_absent_from_dump: usize,
    /// Post-scan Action API race closure.
    pub closure: DumpClosureReport,
}

/// Completed progressive whole-main-namespace bootstrap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WholeEditionBootstrapReport {
    /// Successful bootstrap synchronization boundary.
    pub status: SyncRunStatus,
    /// Durable authenticated dump progress and snapshot identity.
    pub import: WholeEditionImportStatus,
    /// Durable fixed-window RecentChanges discovery and application state.
    pub discovery: WholeEditionDiscoveryStatus,
    /// Whether an interrupted run/import was resumed.
    pub resumed: bool,
    /// Current dump pages newly captured by this invocation.
    pub pages_imported: usize,
    /// Current dump pages whose canonical revision was already durable.
    pub pages_reused: usize,
}

/// Completed overlap-window update for a bootstrapped whole edition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WholeEditionUpdateReport {
    /// Successful update synchronization boundary.
    pub status: SyncRunStatus,
    /// Durable RecentChanges discovery and application state.
    pub discovery: WholeEditionDiscoveryStatus,
    /// Whether an interrupted update was resumed.
    pub resumed: bool,
}

/// Applies a bounded, durable RecentChanges overlap window to a whole edition.
pub async fn update_whole_main_namespace(
    client: &MediaWikiClient,
    library: &mut Library,
    collection_id: CollectionId,
) -> Result<WholeEditionUpdateReport, DumpBootstrapError> {
    repair_missing_sync_manifests(library)?;
    let configuration = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
    if configuration.rule != CollectionRule::WholeMainNamespace {
        return Err(DumpBootstrapError::WholeEditionCollectionRequired);
    }
    let wiki = library
        .wiki(configuration.wiki_id)?
        .ok_or(StoreError::WikiNotFound(configuration.wiki_id))?;
    let stored_endpoint =
        Url::parse(&wiki.api_endpoint).map_err(|_| DumpBootstrapError::InvalidSourceUrl)?;
    if client.endpoint() != &stored_endpoint {
        return Err(DumpBootstrapError::ClientEndpointMismatch);
    }
    let checkpoint = library.sync_checkpoints()?.into_iter().find(|checkpoint| {
        checkpoint.wiki_id == configuration.wiki_id
            && checkpoint.collection_id == Some(collection_id)
            && checkpoint.last_run_id.is_some()
    });
    let Some(checkpoint) = checkpoint else {
        return Err(DumpBootstrapError::WholeEditionBootstrapRequired);
    };
    let source_now = client.source_timestamp().await?;
    let candidate = mediawiki_timestamp_seconds(&source_now)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(DumpBootstrapError::InvalidRaceWindowTimestamp)?;
    if candidate.saturating_sub(checkpoint.committed_through)
        > MAX_SAFE_RECENT_CHANGES_WINDOW_SECONDS
    {
        library.mark_whole_edition_long_gap(
            collection_id,
            checkpoint.committed_through,
            candidate,
            "source checkpoint exceeds the safe RecentChanges recovery window",
        )?;
        return Err(DumpBootstrapError::WholeEditionLongGap {
            last_safe_checkpoint: checkpoint.committed_through,
            source_now: candidate,
        });
    }
    let started = library.start_or_resume_sync_run(
        configuration.wiki_id,
        Some(collection_id),
        SyncRunKind::Update,
        candidate,
    )?;
    let claimed =
        library.claim_or_resume_whole_edition_discovery(WholeEditionDiscoveryRequest {
            run_id: started.status.run_id,
            kind: WholeEditionDiscoveryKind::Incremental,
            window_start: started.status.window_start,
            window_end: started.status.checkpoint_candidate,
            import_id: None,
            recovery_marker_id: None,
        })?;
    let discovery_id = claimed.status.discovery_id;
    let discovered = discover_whole_edition_changes(
        client,
        library,
        claimed.status,
        &format_mediawiki_timestamp(started.status.window_start)?,
        &format_mediawiki_timestamp(started.status.checkpoint_candidate)?,
    )
    .await;
    let discovered = match discovered {
        Ok(status) => status,
        Err(error) => {
            library.fail_whole_edition_discovery(
                discovery_id,
                error.code(),
                &error.to_string(),
                error.is_retryable(),
            )?;
            return Err(error);
        }
    };
    let discovery = match apply_whole_edition_changes(
        client,
        library,
        configuration.wiki_id,
        collection_id,
        discovered,
    )
    .await
    {
        Ok(status) => status,
        Err(error) => {
            library.fail_whole_edition_discovery(
                discovery_id,
                error.code(),
                &error.to_string(),
                error.is_retryable(),
            )?;
            return Err(error);
        }
    };
    let status = library.complete_sync_run(started.status.run_id, None)?;
    library.append_sync_manifest(status.run_id)?;
    Ok(WholeEditionUpdateReport {
        status,
        discovery,
        resumed: started.resumed || claimed.resumed,
    })
}

/// Authenticates a dump index, then alternates one artifact download with one
/// streaming import for a whole-main-namespace collection.
///
/// Every page is indexed before its durable import cursor advances, so local readers
/// and search can use completed pages while later artifacts are still transferring.
/// A fixed source-clock RecentChanges window is durably discovered before the dump
/// scan and applied before the synchronization checkpoint and manifest complete.
#[allow(clippy::too_many_arguments)]
pub async fn bootstrap_whole_main_namespace_from_trusted_dump(
    client: &MediaWikiClient,
    library: &mut Library,
    collection_id: CollectionId,
    trust: &TrustedDumpIndex,
    cache_directory: &Path,
    acquisition_limits: DumpAcquisitionLimits,
    limits: DumpLimits,
) -> Result<WholeEditionBootstrapReport, DumpBootstrapError> {
    repair_missing_sync_manifests(library)?;
    let configuration = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
    if configuration.rule != CollectionRule::WholeMainNamespace {
        return Err(DumpBootstrapError::WholeEditionCollectionRequired);
    }
    if configuration.history_policy != HistoryPolicy::CurrentAndFuture {
        return Err(DumpBootstrapError::UnsupportedHistoryPolicy(
            configuration.history_policy,
        ));
    }
    let wiki = library
        .wiki(configuration.wiki_id)?
        .ok_or(StoreError::WikiNotFound(configuration.wiki_id))?;
    let stored_endpoint =
        Url::parse(&wiki.api_endpoint).map_err(|_| DumpBootstrapError::InvalidSourceUrl)?;
    if client.endpoint() != &stored_endpoint {
        return Err(DumpBootstrapError::ClientEndpointMismatch);
    }

    // Authenticating the small index first yields the exact ordered transfer plan;
    // artifact bytes remain deferred until the import loop below.
    let inventory = client
        .acquire_current_dump_inventory(trust, acquisition_limits)
        .await?;
    if inventory.artifact_count() == 0 {
        return Err(DumpBootstrapError::EmptyDumpSet);
    }
    let compressed_bytes = inventory.total_compressed_bytes()?;
    if compressed_bytes > limits.max_compressed_bytes {
        return Err(DumpError::CompressedLimitExceeded {
            limit: limits.max_compressed_bytes,
        }
        .into());
    }
    let snapshot_timestamp = mediawiki_timestamp_seconds(inventory.generated_at())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(DumpBootstrapError::InvalidSnapshotTimestamp)?;
    let source_now = client.source_timestamp().await?;
    let candidate = mediawiki_timestamp_seconds(&source_now)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(DumpBootstrapError::InvalidRaceWindowTimestamp)?;
    if candidate < snapshot_timestamp {
        return Err(DumpBootstrapError::RaceWindowPrecedesSnapshot);
    }
    let started = library.start_or_resume_sync_run(
        configuration.wiki_id,
        Some(collection_id),
        SyncRunKind::Bootstrap,
        candidate,
    )?;
    let race_window_end = started.status.checkpoint_candidate;
    let recovery_marker = library.whole_edition_recovery_marker(collection_id)?;
    let snapshot_id = format!("{}@{}", inventory.database_name(), inventory.generated_at());
    let import = library.claim_or_resume_whole_edition_import(WholeEditionImportRequest {
        run_id: started.status.run_id,
        snapshot_id: &snapshot_id,
        dump_digest: &format!("b3:{}", inventory.index_digest().to_hex()),
        dump_compressed_bytes: compressed_bytes,
        collection_generation: configuration.generation,
        snapshot_timestamp,
        race_window_end,
        recovery_marker_id: recovery_marker
            .as_ref()
            .map(|marker| marker.recovery_marker_id),
    })?;
    let discovery_kind = if recovery_marker.is_some() {
        WholeEditionDiscoveryKind::LongGapClosure
    } else {
        WholeEditionDiscoveryKind::RaceWindow
    };
    let discovery =
        library.claim_or_resume_whole_edition_discovery(WholeEditionDiscoveryRequest {
            run_id: started.status.run_id,
            kind: discovery_kind,
            window_start: snapshot_timestamp,
            window_end: race_window_end,
            import_id: Some(import.status.dump.import_id),
            recovery_marker_id: recovery_marker
                .as_ref()
                .map(|marker| marker.recovery_marker_id),
        })?;
    let discovery_id = discovery.status.discovery_id;
    let discovery = discover_whole_edition_changes(
        client,
        library,
        discovery.status,
        inventory.generated_at(),
        &format_mediawiki_timestamp(race_window_end)?,
    )
    .await;
    let mut discovery = match discovery {
        Ok(status) => status,
        Err(error) => {
            library.fail_whole_edition_discovery(
                discovery_id,
                error.code(),
                &error.to_string(),
                error.is_retryable(),
            )?;
            return Err(error);
        }
    };

    let scan = stream_whole_edition_inventory(
        client,
        library,
        configuration.wiki_id,
        collection_id,
        &inventory,
        cache_directory,
        acquisition_limits,
        limits,
        import.status.dump.import_id,
        import.status.dump.pages_scanned,
        &wiki.api_endpoint,
        &wiki.language_code,
        &mut discovery,
    )
    .await;
    let (pages_imported, pages_reused, pages_scanned) = match scan {
        Ok(report) => report,
        Err(error) => {
            library.fail_dump_import(
                import.status.dump.import_id,
                error.code(),
                &error.to_string(),
                error.is_retryable(),
            )?;
            return Err(error);
        }
    };
    library.complete_dump_import(import.status.dump.import_id, pages_scanned)?;

    let discovery = apply_whole_edition_changes(
        client,
        library,
        configuration.wiki_id,
        collection_id,
        discovery,
    )
    .await;
    let discovery = match discovery {
        Ok(status) => status,
        Err(error) => {
            library.fail_whole_edition_discovery(
                discovery_id,
                error.code(),
                &error.to_string(),
                error.is_retryable(),
            )?;
            return Err(error);
        }
    };
    if let Some(marker) = recovery_marker {
        library
            .resolve_whole_edition_long_gap(marker.recovery_marker_id, discovery.discovery_id)?;
    }
    let status = library.complete_sync_run(started.status.run_id, None)?;
    library.append_sync_manifest(status.run_id)?;
    let import = library
        .whole_edition_import_status(import.status.dump.import_id)?
        .ok_or(StoreError::CorruptMetadata(
            "whole-edition import disappeared after completion",
        ))?;
    let resumed = started.resumed || import.dump.attempt_count > 1;
    Ok(WholeEditionBootstrapReport {
        status,
        import,
        discovery,
        resumed,
        pages_imported,
        pages_reused,
    })
}

#[allow(clippy::too_many_arguments)]
async fn stream_whole_edition_inventory(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    inventory: &VerifiedDumpInventory,
    cache_directory: &Path,
    acquisition_limits: DumpAcquisitionLimits,
    limits: DumpLimits,
    import_id: u64,
    resume_after_pages: u64,
    api_endpoint: &str,
    language_code: &str,
    discovery: &mut WholeEditionDiscoveryStatus,
) -> Result<(usize, usize, u64), DumpBootstrapError> {
    let mut pages_imported = 0_usize;
    let mut pages_reused = 0_usize;
    let mut pages_before_artifact = 0_u64;
    let mut decompressed_before_artifact = 0_u64;
    let mut search_index = SqliteSearchIndex::open(library)?;
    for artifact_index in 0..inventory.artifact_count() {
        // A completed cache entry is re-opened and re-hashed; otherwise only this
        // one part is transferred before any later part is contacted.
        let artifact = client
            .acquire_current_dump_artifact(
                inventory,
                artifact_index,
                cache_directory,
                acquisition_limits,
            )
            .await?;
        let artifact_limits =
            remaining_set_limits(limits, pages_before_artifact, decompressed_before_artifact)?;
        let mut reader = DumpReader::new(artifact.open()?, artifact_limits)?;
        validate_site_info(
            reader.site_info(),
            inventory.database_name(),
            api_endpoint,
            language_code,
        )?;
        while let Some(page) = reader.next() {
            let page = page?;
            let pages_scanned = pages_before_artifact
                .checked_add(reader.pages_examined())
                .ok_or(DumpBootstrapError::PageCursorOverflow)?;
            enforce_global_page_limit(pages_scanned, limits.max_pages)?;
            if pages_scanned <= resume_after_pages {
                continue;
            }
            let newly_captured = import_dump_page(library, wiki_id, collection_id, &page)?;
            let stored = library
                .page(wiki_id, page.page_id)?
                .ok_or(StoreError::PageNotFound {
                    wiki_id,
                    page_id: page.page_id,
                })?;
            let revision_id = stored
                .current_revision_id
                .ok_or(CaptureError::MissingLocalPageHead(page.page_id))?;
            index_stored_current_revision_with(
                library,
                &mut search_index,
                wiki_id,
                stored.page_id,
                &stored.title,
                revision_id,
            )?;
            // Cursor-after-index is the progressive availability boundary.
            library.record_whole_edition_imported_member(
                import_id,
                pages_scanned,
                page.page_id,
                page.revision.metadata.revision_id,
                page.revision
                    .source
                    .as_ref()
                    .map_or(0, |source| source.len() as u64),
            )?;
            if newly_captured {
                pages_imported += 1;
            } else {
                pages_reused += 1;
            }
        }
        pages_before_artifact = pages_before_artifact
            .checked_add(reader.pages_examined())
            .ok_or(DumpBootstrapError::PageCursorOverflow)?;
        enforce_global_page_limit(pages_before_artifact, limits.max_pages)?;
        decompressed_before_artifact = decompressed_before_artifact
            .checked_add(reader.decompressed_bytes_read())
            .ok_or(DumpBootstrapError::DecompressedByteCounterOverflow)?;
        if decompressed_before_artifact > limits.max_decompressed_bytes {
            return Err(DumpError::DecompressedLimitExceeded {
                limit: limits.max_decompressed_bytes,
            }
            .into());
        }
        library.record_whole_edition_stream_progress(
            import_id,
            pages_before_artifact,
            u64::try_from(artifact_index + 1)
                .map_err(|_| DumpBootstrapError::ArtifactLengthOverflow)?,
            0,
        )?;
        tail_whole_edition_race_window(client, library, discovery).await?;
    }
    if pages_before_artifact < resume_after_pages {
        return Err(DumpBootstrapError::ResumeCursorBeyondDump {
            stored: resume_after_pages,
            actual: pages_before_artifact,
        });
    }
    Ok((pages_imported, pages_reused, pages_before_artifact))
}

async fn tail_whole_edition_race_window(
    client: &MediaWikiClient,
    library: &mut Library,
    discovery: &mut WholeEditionDiscoveryStatus,
) -> Result<(), DumpBootstrapError> {
    let source_now = client.source_timestamp().await?;
    let new_end = mediawiki_timestamp_seconds(&source_now)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(DumpBootstrapError::InvalidRaceWindowTimestamp)?;
    if new_end <= discovery.window_end {
        return Ok(());
    }
    let old_end = discovery.window_end;
    *discovery =
        library.extend_whole_edition_race_window(discovery.discovery_id, old_end, new_end)?;
    *discovery = discover_whole_edition_changes(
        client,
        library,
        discovery.clone(),
        &format_mediawiki_timestamp(old_end)?,
        &format_mediawiki_timestamp(new_end)?,
    )
    .await?;
    Ok(())
}

async fn discover_whole_edition_changes(
    client: &MediaWikiClient,
    library: &mut Library,
    mut status: WholeEditionDiscoveryStatus,
    window_start: &str,
    window_end: &str,
) -> Result<WholeEditionDiscoveryStatus, DumpBootstrapError> {
    while !status.source_exhausted {
        let continuation = status
            .continuation
            .as_deref()
            .map(decode_recent_changes_continuation)
            .transpose()?;
        let batch = client
            .recent_changes_batch(window_start, Some(window_end), continuation.as_ref())
            .await?;
        let mut changes = Vec::with_capacity(batch.changes.len());
        for change in &batch.changes {
            let occurred_at = mediawiki_timestamp_seconds(&change.timestamp)
                .and_then(|value| u64::try_from(value).ok())
                .ok_or(DumpBootstrapError::InvalidRecentChangeTimestamp(
                    change.change_id,
                ))?;
            changes.push(WholeEditionChange {
                change_id: change.change_id,
                kind: stored_change_kind(change.kind),
                occurred_at,
                page_id: change.page_id,
                revision_id: change.revision_id,
                namespace: Some(change.namespace),
                title: Some(change.title.as_str()),
            });
        }
        let next_cursor = batch
            .continuation
            .as_ref()
            .map(encode_recent_changes_continuation);
        status = library.record_whole_edition_recent_changes_batch(
            status.discovery_id,
            status.batches_recorded,
            next_cursor.as_deref(),
            &changes,
        )?;
    }
    Ok(status)
}

async fn apply_whole_edition_changes(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    mut status: WholeEditionDiscoveryStatus,
) -> Result<WholeEditionDiscoveryStatus, DumpBootstrapError> {
    loop {
        let changes = library.whole_edition_pending_changes_after(
            status.discovery_id,
            None,
            WHOLE_EDITION_CHANGE_PAGE_SIZE,
        )?;
        if changes.is_empty() {
            break;
        }
        for change in changes {
            let disposition = if change
                .namespace
                .is_some_and(|namespace| namespace != MAIN_NAMESPACE)
            {
                WholeEditionChangeDisposition::Ignored
            } else {
                apply_whole_edition_change(client, library, wiki_id, collection_id, &change).await?
            };
            status = library.mark_whole_edition_change_applied(
                status.discovery_id,
                change.change_id,
                disposition,
            )?;
        }
    }
    library
        .complete_whole_edition_discovery(status.discovery_id)
        .map_err(Into::into)
}

async fn apply_whole_edition_change(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    change: &StoredWholeEditionChange,
) -> Result<WholeEditionChangeDisposition, DumpBootstrapError> {
    match change.kind {
        WholeEditionChangeKind::Edit | WholeEditionChangeKind::New => {
            let (Some(page_id), Some(revision_id), Some(title)) =
                (change.page_id, change.revision_id, change.title.as_deref())
            else {
                return Ok(WholeEditionChangeDisposition::Ignored);
            };
            let title = PageTitle::new(title)
                .map_err(|_| DumpBootstrapError::InvalidRecentChangeTitle(change.change_id))?;
            let content = client.revision_content(page_id, revision_id).await?;
            validate_content(&content.metadata, &content.source)?;
            if library.revision(wiki_id, revision_id)?.is_none() {
                enforce_collection_byte_budget(
                    library,
                    collection_id,
                    content.source.len() as u64,
                )?;
                if library.page(wiki_id, page_id)?.is_some() {
                    library.capture_revision(
                        wiki_id,
                        page_id,
                        &revision_capture(&content.metadata, &content.source),
                    )?;
                } else {
                    library.capture_current_revision(
                        wiki_id,
                        collection_id,
                        &CurrentRevisionCapture {
                            page_id,
                            namespace: MAIN_NAMESPACE,
                            title: &title,
                            revision_id,
                            parent_id: content.metadata.parent_id,
                            timestamp: &content.metadata.timestamp,
                            author: content.metadata.user.as_deref(),
                            author_id: content.metadata.user_id,
                            comment: content.metadata.comment.as_deref(),
                            minor: content.metadata.minor,
                            upstream_sha1: content.metadata.sha1.as_deref(),
                            content_model: content
                                .metadata
                                .content_model
                                .as_deref()
                                .expect("validated RecentChanges content model"),
                            source: &content.source,
                        },
                    )?;
                }
            }
            library.reconcile_current_revision(
                wiki_id,
                collection_id,
                page_id,
                MAIN_NAMESPACE,
                &title,
                revision_id,
            )?;
            index_stored_current_revision(library, wiki_id, page_id, &title, revision_id)?;
            let image_policy = library
                .collection_configuration(collection_id)?
                .ok_or(StoreError::CollectionNotConfigured(collection_id))?
                .image_policy;
            capture_revision_media(
                client,
                library,
                wiki_id,
                collection_id,
                page_id,
                revision_id,
                image_policy,
            )
            .await?;
            Ok(WholeEditionChangeDisposition::Applied)
        }
        WholeEditionChangeKind::Delete => {
            let Some(page_id) = change_page_id(library, wiki_id, change)? else {
                return Ok(WholeEditionChangeDisposition::Ignored);
            };
            if library.page(wiki_id, page_id)?.is_some() {
                library.mark_page_missing(wiki_id, collection_id, page_id)?;
                Ok(WholeEditionChangeDisposition::Applied)
            } else {
                Ok(WholeEditionChangeDisposition::Ignored)
            }
        }
        WholeEditionChangeKind::Move | WholeEditionChangeKind::Restore => {
            let Some(page_id) = change_page_id(library, wiki_id, change)? else {
                return Ok(WholeEditionChangeDisposition::Ignored);
            };
            match library.page(wiki_id, page_id)? {
                Some(stored) => {
                    reconcile_page_head(
                        client,
                        library,
                        wiki_id,
                        collection_id,
                        &stored,
                        ReconciliationLimits::default(),
                        &NeverCancelled,
                    )
                    .await?;
                    Ok(WholeEditionChangeDisposition::Applied)
                }
                None => match client.resolve_page_head(page_id).await? {
                    PageHeadResolution::Missing { .. } => {
                        Ok(WholeEditionChangeDisposition::Ignored)
                    }
                    PageHeadResolution::Found(page) => {
                        let mut search = SqliteSearchIndex::open(library)?;
                        capture_resolved_page_head(
                            client,
                            library,
                            &mut search,
                            wiki_id,
                            collection_id,
                            *page,
                        )
                        .await?;
                        Ok(WholeEditionChangeDisposition::Applied)
                    }
                },
            }
        }
        WholeEditionChangeKind::Other => Ok(WholeEditionChangeDisposition::Ignored),
    }
}

fn change_page_id(
    library: &Library,
    wiki_id: WikiId,
    change: &StoredWholeEditionChange,
) -> Result<Option<PageId>, DumpBootstrapError> {
    if change.page_id.is_some() {
        return Ok(change.page_id);
    }
    let Some(title) = change.title.as_deref() else {
        return Ok(None);
    };
    let title = PageTitle::new(title)
        .map_err(|_| DumpBootstrapError::InvalidRecentChangeTitle(change.change_id))?;
    Ok(library
        .pages_by_title(&title, Some(wiki_id))?
        .into_iter()
        .find(|page| page.title == title)
        .map(|page| page.page_id))
}

fn stored_change_kind(kind: RecentChangeKind) -> WholeEditionChangeKind {
    match kind {
        RecentChangeKind::Edit => WholeEditionChangeKind::Edit,
        RecentChangeKind::New => WholeEditionChangeKind::New,
        RecentChangeKind::Move => WholeEditionChangeKind::Move,
        RecentChangeKind::Delete => WholeEditionChangeKind::Delete,
        RecentChangeKind::Restore => WholeEditionChangeKind::Restore,
    }
}

fn encode_recent_changes_continuation(continuation: &RecentChangesContinuation) -> String {
    format!(
        "{}:{}{}",
        continuation.generic().len(),
        continuation.generic(),
        continuation.recent_changes()
    )
}

fn decode_recent_changes_continuation(
    encoded: &str,
) -> Result<RecentChangesContinuation, DumpBootstrapError> {
    let (length, values) = encoded
        .split_once(':')
        .ok_or(DumpBootstrapError::InvalidRecentChangesContinuation)?;
    let length = length
        .parse::<usize>()
        .map_err(|_| DumpBootstrapError::InvalidRecentChangesContinuation)?;
    let generic = values
        .get(..length)
        .ok_or(DumpBootstrapError::InvalidRecentChangesContinuation)?;
    let recent_changes = values
        .get(length..)
        .filter(|value| !value.is_empty())
        .ok_or(DumpBootstrapError::InvalidRecentChangesContinuation)?;
    RecentChangesContinuation::from_parts(generic, recent_changes)
        .map_err(DumpBootstrapError::Source)
}

fn format_mediawiki_timestamp(seconds: u64) -> Result<String, DumpBootstrapError> {
    let seconds =
        i64::try_from(seconds).map_err(|_| DumpBootstrapError::InvalidRaceWindowTimestamp)?;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if !(0..=9_999).contains(&year) {
        return Err(DumpBootstrapError::InvalidRaceWindowTimestamp);
    }
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Streams a current-page dump into one already resolved collection, then queries
/// every selected stable page ID through the Action API before committing the run.
///
/// Only `CurrentAndFuture` collections are accepted. The dump scan never resolves or
/// trusts titles as selection input: it imports records only when their stable page
/// IDs occur in the collection's durable resolved membership. The Bootstrap run,
/// dump progress, closure jobs, checkpoint, and manifest are all durable and
/// idempotent.
pub async fn bootstrap_collection_from_verified_dump(
    client: &MediaWikiClient,
    library: &mut Library,
    collection_id: CollectionId,
    dump_set: &VerifiedDumpSet,
    limits: DumpLimits,
) -> Result<DumpBootstrapReport, DumpBootstrapError> {
    repair_missing_sync_manifests(library)?;
    let configuration = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
    if configuration.history_policy != HistoryPolicy::CurrentAndFuture {
        return Err(DumpBootstrapError::UnsupportedHistoryPolicy(
            configuration.history_policy,
        ));
    }
    let members = library.resolved_collection_members(collection_id)?;
    enforce_page_budget(
        configuration
            .budget
            .maximum_pages()
            .map(|limit| limit.get()),
        members.len(),
    )?;
    let wiki = library
        .wiki(configuration.wiki_id)?
        .ok_or(StoreError::WikiNotFound(configuration.wiki_id))?;
    let stored_endpoint =
        Url::parse(&wiki.api_endpoint).map_err(|_| DumpBootstrapError::InvalidSourceUrl)?;
    if client.endpoint() != &stored_endpoint {
        return Err(DumpBootstrapError::ClientEndpointMismatch);
    }
    let compressed_bytes = dump_set
        .artifacts()
        .iter()
        .try_fold(0_u64, |total, artifact| {
            total.checked_add(artifact.length())
        })
        .ok_or(DumpBootstrapError::ArtifactLengthOverflow)?;
    if dump_set.artifacts().is_empty() {
        return Err(DumpBootstrapError::EmptyDumpSet);
    }
    if compressed_bytes > limits.max_compressed_bytes {
        return Err(DumpError::CompressedLimitExceeded {
            limit: limits.max_compressed_bytes,
        }
        .into());
    }
    let dump_digest = format!("b3:{}", dump_set.index_digest().to_hex());

    let candidate = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::ClockBeforeUnixEpoch)?
        .as_secs();
    let started = library.start_or_resume_sync_run(
        configuration.wiki_id,
        Some(collection_id),
        SyncRunKind::Bootstrap,
        candidate,
    )?;
    let bootstrap_started_at = started.status.checkpoint_candidate;
    let dump = library.claim_or_resume_dump_import(DumpImportRequest {
        run_id: started.status.run_id,
        dump_digest: &dump_digest,
        dump_compressed_bytes: compressed_bytes,
        collection_generation: configuration.generation,
        bootstrap_started_at,
    })?;

    let import_id = dump.status.import_id;
    if dump.status.state == DumpImportState::Succeeded {
        let verified = verify_completed_dump_set(
            dump_set,
            limits,
            &wiki.api_endpoint,
            &wiki.language_code,
            &members,
        );
        let (pages_scanned, pages_absent_from_dump) = match verified {
            Ok(verified) => verified,
            Err(error) => {
                library.cancel_sync_run(started.status.run_id)?;
                return Err(error);
            }
        };
        if pages_scanned != dump.status.pages_scanned {
            library.cancel_sync_run(started.status.run_id)?;
            return Err(DumpBootstrapError::CompletedCursorMismatch {
                stored: dump.status.pages_scanned,
                actual: pages_scanned,
            });
        }
        let status = library.complete_sync_run(started.status.run_id, None)?;
        library.append_sync_manifest(status.run_id)?;
        return Ok(DumpBootstrapReport {
            status,
            import: dump.status,
            resumed: true,
            pages_imported: 0,
            pages_reused: 0,
            pages_absent_from_dump,
            closure: DumpClosureReport::default(),
        });
    }
    let result = run_dump_and_closure(
        client,
        library,
        collection_id,
        configuration.wiki_id,
        dump_set,
        limits,
        &wiki.api_endpoint,
        &wiki.language_code,
        &members,
        dump.status.pages_scanned,
        import_id,
        started.status.run_id,
    )
    .await;

    let (pages_imported, pages_reused, pages_absent_from_dump, pages_scanned, closure) =
        match result {
            Ok(report) => report,
            Err(error) => {
                let retryable = error.is_retryable();
                library.fail_dump_import(import_id, error.code(), &error.to_string(), retryable)?;
                return Err(error);
            }
        };

    let import = library.complete_dump_import(import_id, pages_scanned)?;
    let status = library.complete_sync_run(started.status.run_id, None)?;
    library.append_sync_manifest(status.run_id)?;
    Ok(DumpBootstrapReport {
        status,
        import,
        resumed: started.resumed || dump.resumed,
        pages_imported,
        pages_reused,
        pages_absent_from_dump,
        closure,
    })
}

fn verify_completed_dump_set(
    dump_set: &VerifiedDumpSet,
    limits: DumpLimits,
    api_endpoint: &str,
    language_code: &str,
    members: &[wikisync_store::ResolvedCollectionMember],
) -> Result<(u64, usize), DumpBootstrapError> {
    let mut selected = members
        .iter()
        .map(|member| (member.page_id, false))
        .collect::<BTreeMap<_, _>>();
    let mut pages_scanned = 0_u64;
    let mut decompressed_bytes = 0_u64;
    for artifact in dump_set.artifacts() {
        let artifact_limits = remaining_set_limits(limits, pages_scanned, decompressed_bytes)?;
        let mut reader = DumpReader::new(artifact.open()?, artifact_limits)?;
        validate_site_info(
            reader.site_info(),
            dump_set.database_name(),
            api_endpoint,
            language_code,
        )?;
        // We need the reader's examined-page counter after each yielded record;
        // filtered records can make it advance by more than one.
        #[allow(clippy::while_let_on_iterator)]
        while let Some(page) = reader.next() {
            let page = page?;
            let current_pages = pages_scanned
                .checked_add(reader.pages_examined())
                .ok_or(DumpBootstrapError::PageCursorOverflow)?;
            enforce_global_page_limit(current_pages, limits.max_pages)?;
            if let Some(found) = selected.get_mut(&page.page_id) {
                if *found {
                    return Err(DumpBootstrapError::DuplicateSelectedPage(page.page_id));
                }
                *found = true;
            }
        }
        decompressed_bytes = decompressed_bytes
            .checked_add(reader.decompressed_bytes_read())
            .ok_or(DumpBootstrapError::DecompressedByteCounterOverflow)?;
        if decompressed_bytes > limits.max_decompressed_bytes {
            return Err(DumpError::DecompressedLimitExceeded {
                limit: limits.max_decompressed_bytes,
            }
            .into());
        }
        pages_scanned = pages_scanned
            .checked_add(reader.pages_examined())
            .ok_or(DumpBootstrapError::PageCursorOverflow)?;
        enforce_global_page_limit(pages_scanned, limits.max_pages)?;
    }
    Ok((
        pages_scanned,
        selected.values().filter(|found| !**found).count(),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_dump_and_closure(
    client: &MediaWikiClient,
    library: &mut Library,
    collection_id: CollectionId,
    wiki_id: WikiId,
    dump_set: &VerifiedDumpSet,
    limits: DumpLimits,
    api_endpoint: &str,
    language_code: &str,
    members: &[wikisync_store::ResolvedCollectionMember],
    resume_after_pages: u64,
    import_id: u64,
    run_id: u64,
) -> Result<(usize, usize, usize, u64, DumpClosureReport), DumpBootstrapError> {
    let selected = members
        .iter()
        .map(|member| (member.page_id, false))
        .collect::<BTreeMap<_, _>>();
    let mut selected = selected;
    let mut pages_imported = 0;
    let mut pages_reused = 0;
    let mut pages_before_artifact = 0_u64;
    let mut decompressed_before_artifact = 0_u64;
    for artifact in dump_set.artifacts() {
        let input = artifact.open()?;
        let artifact_limits =
            remaining_set_limits(limits, pages_before_artifact, decompressed_before_artifact)?;
        let mut reader = DumpReader::new(input, artifact_limits)?;
        validate_site_info(
            reader.site_info(),
            dump_set.database_name(),
            api_endpoint,
            language_code,
        )?;
        while let Some(page) = reader.next() {
            let page = page?;
            let pages_scanned = pages_before_artifact
                .checked_add(reader.pages_examined())
                .ok_or(DumpBootstrapError::PageCursorOverflow)?;
            enforce_global_page_limit(pages_scanned, limits.max_pages)?;
            if pages_scanned <= resume_after_pages {
                if let Some(found) = selected.get_mut(&page.page_id) {
                    if *found {
                        return Err(DumpBootstrapError::DuplicateSelectedPage(page.page_id));
                    }
                    *found = true;
                }
                continue;
            }
            let Some(found) = selected.get_mut(&page.page_id) else {
                library.record_dump_import_progress(import_id, pages_scanned)?;
                continue;
            };
            if *found {
                return Err(DumpBootstrapError::DuplicateSelectedPage(page.page_id));
            }
            *found = true;
            let newly_captured = import_dump_page(library, wiki_id, collection_id, &page)?;
            let stored = library
                .page(wiki_id, page.page_id)?
                .ok_or(StoreError::PageNotFound {
                    wiki_id,
                    page_id: page.page_id,
                })?;
            let revision_id = stored
                .current_revision_id
                .ok_or(CaptureError::MissingLocalPageHead(page.page_id))?;
            index_stored_current_revision(
                library,
                wiki_id,
                stored.page_id,
                &stored.title,
                revision_id,
            )?;
            // The durable dump cursor advances only after the derived search row is
            // installed. A crash before this point replays the idempotent capture and
            // indexing work instead of skipping a page that was never searchable.
            library.record_dump_imported_page(
                import_id,
                pages_scanned,
                page.page_id,
                page.revision.metadata.revision_id,
                page.revision
                    .source
                    .as_ref()
                    .map_or(0, |source| source.len() as u64),
            )?;
            if newly_captured {
                pages_imported += 1;
            } else {
                pages_reused += 1;
            }
        }
        pages_before_artifact = pages_before_artifact
            .checked_add(reader.pages_examined())
            .ok_or(DumpBootstrapError::PageCursorOverflow)?;
        enforce_global_page_limit(pages_before_artifact, limits.max_pages)?;
        decompressed_before_artifact = decompressed_before_artifact
            .checked_add(reader.decompressed_bytes_read())
            .ok_or(DumpBootstrapError::DecompressedByteCounterOverflow)?;
        if decompressed_before_artifact > limits.max_decompressed_bytes {
            return Err(DumpError::DecompressedLimitExceeded {
                limit: limits.max_decompressed_bytes,
            }
            .into());
        }
        library.record_dump_import_progress(import_id, pages_before_artifact)?;
    }
    let final_pages_scanned = pages_before_artifact;
    if final_pages_scanned < resume_after_pages {
        return Err(DumpBootstrapError::ResumeCursorBeyondDump {
            stored: resume_after_pages,
            actual: final_pages_scanned,
        });
    }
    let pages_absent_from_dump = selected.values().filter(|found| !**found).count();
    for member in members {
        library.enqueue_sync_job(
            run_id,
            &format!("dump-race-close:{}", member.page_id),
            "dump-race-close-page",
            Some(&member.page_id.to_string()),
        )?;
    }
    let closure =
        close_race_window(client, library, wiki_id, collection_id, run_id, members).await?;
    Ok((
        pages_imported,
        pages_reused,
        pages_absent_from_dump,
        final_pages_scanned,
        closure,
    ))
}

fn import_dump_page(
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    page: &DumpPage,
) -> Result<bool, DumpBootstrapError> {
    if page.namespace != MAIN_NAMESPACE {
        return Err(DumpBootstrapError::InvalidPageNamespace {
            page_id: page.page_id,
            namespace: page.namespace,
        });
    }
    if page.revision.content_format != "text/x-wiki" {
        return Err(DumpBootstrapError::UnsupportedContentFormat {
            revision_id: page.revision.metadata.revision_id,
            format: page.revision.content_format.clone(),
        });
    }
    let source = page
        .revision
        .source
        .as_deref()
        .ok_or(DumpBootstrapError::SuppressedRevision(
            page.revision.metadata.revision_id,
        ))?;
    super::parse_mediawiki_timestamp(
        &page.revision.metadata.timestamp,
        page.revision.metadata.revision_id,
    )?;
    validate_content(&page.revision.metadata, source)?;
    let newly_captured = library
        .revision(wiki_id, page.revision.metadata.revision_id)?
        .is_none();
    if newly_captured {
        enforce_collection_byte_budget(library, collection_id, source.len() as u64)?;
    }
    let metadata = &page.revision.metadata;
    library.capture_current_revision(
        wiki_id,
        collection_id,
        &CurrentRevisionCapture {
            page_id: page.page_id,
            namespace: page.namespace,
            title: &page.title,
            revision_id: metadata.revision_id,
            parent_id: metadata.parent_id,
            timestamp: &metadata.timestamp,
            author: metadata.user.as_deref(),
            author_id: metadata.user_id,
            comment: metadata.comment.as_deref(),
            minor: metadata.minor,
            upstream_sha1: metadata.sha1.as_deref(),
            content_model: metadata
                .content_model
                .as_deref()
                .expect("validated dump content model"),
            source,
        },
    )?;
    Ok(newly_captured)
}

async fn close_race_window(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    run_id: u64,
    members: &[wikisync_store::ResolvedCollectionMember],
) -> Result<DumpClosureReport, DumpBootstrapError> {
    let selected = members
        .iter()
        .map(|member| (member.page_id, member))
        .collect::<BTreeMap<_, _>>();
    let mut report = DumpClosureReport::default();
    let mut search_index = SqliteSearchIndex::open(library)?;
    while let Some(job) = library.claim_next_sync_job(run_id)? {
        if job.kind != "dump-race-close-page" {
            return Err(DumpBootstrapError::InvalidClosureJob);
        }
        let page_id = job
            .subject
            .as_deref()
            .and_then(|subject| subject.parse::<u64>().ok())
            .and_then(|id| PageId::new(id).ok())
            .ok_or(DumpBootstrapError::InvalidClosureJob)?;
        let member = selected
            .get(&page_id)
            .ok_or(DumpBootstrapError::InvalidClosureJob)?;

        let page_result: Result<ClosurePageResult, DumpBootstrapError> = async {
            if let Some(stored_page) = library.page(wiki_id, page_id)? {
                reconcile_page_head(
                    client,
                    library,
                    wiki_id,
                    collection_id,
                    &stored_page,
                    ReconciliationLimits::default(),
                    &NeverCancelled,
                )
                .await
                .map(ClosurePageResult::Reconciled)
                .map_err(Into::into)
            } else {
                match client
                    .resolve_page_head(page_id)
                    .await
                    .map_err(CaptureError::Source)?
                {
                    PageHeadResolution::Missing { .. } => Ok(ClosurePageResult::Missing),
                    PageHeadResolution::Found(page) => {
                        if page.page_id != member.page_id {
                            return Err(DumpBootstrapError::ClosurePageIdentityMismatch {
                                expected: member.page_id,
                                actual: page.page_id,
                            });
                        }
                        let (captured, media) = capture_resolved_page_head(
                            client,
                            library,
                            &mut search_index,
                            wiki_id,
                            collection_id,
                            *page,
                        )
                        .await?;
                        Ok(ClosurePageResult::Captured(captured, media))
                    }
                }
            }
        }
        .await;

        match page_result {
            Ok(ClosurePageResult::Reconciled(page)) => merge_reconciliation(&mut report, page),
            Ok(ClosurePageResult::Captured(captured, media)) => {
                report.pages_checked += 1;
                report.pages_captured_from_api += 1;
                report.differing_heads += usize::from(captured.newly_captured);
                report.media.merge(media);
            }
            Ok(ClosurePageResult::Missing) => {
                report.pages_checked += 1;
                report.missing_pages += 1;
            }
            Err(error) => {
                let retryable = error.is_retryable();
                library.fail_sync_job(job.job_id, error.code(), &error.to_string(), retryable)?;
                return Err(error);
            }
        }
        library.complete_sync_job(job.job_id)?;
    }
    Ok(report)
}

enum ClosurePageResult {
    Reconciled(PageReconciliationReport),
    Captured(CapturedPage, MediaCaptureReport),
    Missing,
}

fn merge_reconciliation(report: &mut DumpClosureReport, page: PageReconciliationReport) {
    report.pages_checked += 1;
    report.differing_heads += usize::from(page.head_differed);
    report.missing_pages += usize::from(page.missing);
    report.revision_batches += page.revision_batches;
    report.revisions_enumerated += page.revisions_enumerated;
    report.revisions_captured += page.revisions_captured;
    report.revisions_reused += page.revisions_reused;
    report.media.merge(page.media);
}

fn enforce_page_budget(limit: Option<u64>, pages: usize) -> Result<(), DumpBootstrapError> {
    let pages = u64::try_from(pages)
        .map_err(|_| StoreError::InvalidConfig("collection member count is too large"))?;
    if let Some(limit) = limit
        && pages > limit
    {
        return Err(StoreError::CollectionBudgetExceeded {
            resource: "pages",
            limit,
            estimated: pages,
        }
        .into());
    }
    Ok(())
}

fn enforce_global_page_limit(pages: u64, limit: u64) -> Result<(), DumpBootstrapError> {
    if pages > limit {
        return Err(DumpError::PageLimitExceeded { limit }.into());
    }
    Ok(())
}

fn remaining_set_limits(
    limits: DumpLimits,
    pages_consumed: u64,
    decompressed_consumed: u64,
) -> Result<DumpLimits, DumpBootstrapError> {
    let remaining_decompressed = limits
        .max_decompressed_bytes
        .checked_sub(decompressed_consumed)
        .ok_or(DumpBootstrapError::DecompressedByteCounterOverflow)?;
    if remaining_decompressed == 0 {
        return Err(DumpError::DecompressedLimitExceeded {
            limit: limits.max_decompressed_bytes,
        }
        .into());
    }
    let remaining_pages = limits
        .max_pages
        .checked_sub(pages_consumed)
        .ok_or(DumpBootstrapError::PageCursorOverflow)?;
    if remaining_pages == 0 {
        return Err(DumpError::PageLimitExceeded {
            limit: limits.max_pages,
        }
        .into());
    }
    Ok(DumpLimits {
        max_decompressed_bytes: remaining_decompressed,
        max_pages: remaining_pages,
        ..limits
    })
}

fn validate_site_info(
    site: &DumpSiteInfo,
    expected_database: &str,
    api_endpoint: &str,
    expected_language: &str,
) -> Result<(), DumpBootstrapError> {
    if site.database_name != expected_database {
        return Err(DumpBootstrapError::DatabaseMismatch {
            expected: expected_database.to_owned(),
            actual: site.database_name.clone(),
        });
    }
    if site.language_code != expected_language {
        return Err(DumpBootstrapError::LanguageMismatch {
            expected: expected_language.to_owned(),
            actual: site.language_code.clone(),
        });
    }
    let main = site
        .namespaces
        .iter()
        .find(|namespace| namespace.key == MAIN_NAMESPACE)
        .ok_or(DumpBootstrapError::MissingMainNamespace)?;
    if !main.name.is_empty() || main.case_rule != site.case_rule {
        return Err(DumpBootstrapError::InvalidMainNamespace);
    }
    let base = site
        .base_url
        .as_deref()
        .ok_or(DumpBootstrapError::MissingBaseUrl)?;
    if !same_origin(api_endpoint, base)? {
        return Err(DumpBootstrapError::SourceOriginMismatch);
    }
    Ok(())
}

fn same_origin(left: &str, right: &str) -> Result<bool, DumpBootstrapError> {
    let left = Url::parse(left).map_err(|_| DumpBootstrapError::InvalidSourceUrl)?;
    let right = Url::parse(right).map_err(|_| DumpBootstrapError::InvalidSourceUrl)?;
    Ok(left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default())
}

/// A dump parse, trust-binding, canonical-capture, or closure failure.
#[derive(Debug)]
pub enum DumpBootstrapError {
    /// Reopening an authenticated artifact failed its acquisition guarantees.
    Acquisition(DumpAcquisitionError),
    /// Bounded dump parsing failed.
    Dump(DumpError),
    /// Existing capture or reconciliation behavior failed.
    Capture(CaptureError),
    /// Bounded RecentChanges or source-clock access failed.
    Source(ClientError),
    /// Durable import/run bookkeeping failed.
    Store(StoreError),
    /// The authenticated set contained no current-page artifacts.
    EmptyDumpSet,
    /// Summing authenticated artifact lengths overflowed.
    ArtifactLengthOverflow,
    /// The global multipart page cursor overflowed.
    PageCursorOverflow,
    /// The multipart decompressed-byte counter overflowed.
    DecompressedByteCounterOverflow,
    /// Dump bootstrap is intentionally current-only.
    UnsupportedHistoryPolicy(HistoryPolicy),
    /// Whole-edition streaming was requested for a selective collection.
    WholeEditionCollectionRequired,
    /// RecentChanges update was requested before a successful dump bootstrap.
    WholeEditionBootstrapRequired,
    /// The committed checkpoint is too old for bounded RecentChanges recovery.
    WholeEditionLongGap {
        last_safe_checkpoint: u64,
        source_now: u64,
    },
    /// The authenticated snapshot timestamp was not a valid nonnegative source time.
    InvalidSnapshotTimestamp,
    /// The fixed source-clock race boundary was invalid.
    InvalidRaceWindowTimestamp,
    /// The fixed race boundary preceded the authenticated snapshot boundary.
    RaceWindowPrecedesSnapshot,
    /// A durable RecentChanges cursor could not be reconstructed.
    InvalidRecentChangesContinuation,
    /// A RecentChanges row carried an invalid timestamp.
    InvalidRecentChangeTimestamp(u64),
    /// A RecentChanges row carried an invalid page title.
    InvalidRecentChangeTitle(u64),
    /// Authenticated index and dump database identities differ.
    DatabaseMismatch { expected: String, actual: String },
    /// Dump and configured source language identities differ.
    LanguageMismatch { expected: String, actual: String },
    /// Dump site metadata omitted the main namespace.
    MissingMainNamespace,
    /// Dump main-namespace metadata was inconsistent with site metadata.
    InvalidMainNamespace,
    /// Dump site metadata omitted its canonical base URL.
    MissingBaseUrl,
    /// Configured API or dump base URL was not valid.
    InvalidSourceUrl,
    /// Dump base URL did not share the configured API origin.
    SourceOriginMismatch,
    /// Action API client and durable wiki configuration were different sources.
    ClientEndpointMismatch,
    /// A selected page escaped the main-namespace filter.
    InvalidPageNamespace { page_id: PageId, namespace: i32 },
    /// A selected current revision was not canonical wikitext.
    UnsupportedContentFormat {
        revision_id: wikisync_core::RevisionId,
        format: String,
    },
    /// A selected dump revision did not expose public canonical source.
    SuppressedRevision(wikisync_core::RevisionId),
    /// Durable resume progress referred beyond the authenticated dump's end.
    ResumeCursorBeyondDump { stored: u64, actual: u64 },
    /// A completed durable import did not cover this exact authenticated set.
    CompletedCursorMismatch { stored: u64, actual: u64 },
    /// The multipart set repeated one selected stable page ID.
    DuplicateSelectedPage(PageId),
    /// Action API returned a different stable identity for a closure query.
    ClosurePageIdentityMismatch { expected: PageId, actual: PageId },
    /// A durable closure job did not belong to the resolved selection.
    InvalidClosureJob,
}

impl DumpBootstrapError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Acquisition(_) => false,
            Self::Capture(error) => error.is_retryable(),
            Self::Source(error) => error.is_retryable(),
            Self::Store(_) => false,
            Self::Dump(_)
            | Self::EmptyDumpSet
            | Self::ArtifactLengthOverflow
            | Self::PageCursorOverflow
            | Self::DecompressedByteCounterOverflow
            | Self::UnsupportedHistoryPolicy(_)
            | Self::WholeEditionCollectionRequired
            | Self::WholeEditionBootstrapRequired
            | Self::WholeEditionLongGap { .. }
            | Self::InvalidSnapshotTimestamp
            | Self::InvalidRaceWindowTimestamp
            | Self::RaceWindowPrecedesSnapshot
            | Self::InvalidRecentChangesContinuation
            | Self::InvalidRecentChangeTimestamp(_)
            | Self::InvalidRecentChangeTitle(_)
            | Self::DatabaseMismatch { .. }
            | Self::LanguageMismatch { .. }
            | Self::MissingMainNamespace
            | Self::InvalidMainNamespace
            | Self::MissingBaseUrl
            | Self::InvalidSourceUrl
            | Self::SourceOriginMismatch
            | Self::ClientEndpointMismatch
            | Self::InvalidPageNamespace { .. }
            | Self::UnsupportedContentFormat { .. }
            | Self::SuppressedRevision(_)
            | Self::ResumeCursorBeyondDump { .. }
            | Self::CompletedCursorMismatch { .. }
            | Self::DuplicateSelectedPage(_)
            | Self::ClosurePageIdentityMismatch { .. }
            | Self::InvalidClosureJob => false,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Acquisition(_) => "dump-artifact-changed",
            Self::Capture(error) => error.code(),
            Self::Source(_) => "dump-recent-changes-source",
            Self::Store(_) => "dump-store",
            Self::Dump(_) => "dump-invalid",
            Self::EmptyDumpSet | Self::ArtifactLengthOverflow => "dump-artifact-set",
            Self::PageCursorOverflow => "dump-page-cursor",
            Self::DecompressedByteCounterOverflow => "dump-decompressed-counter",
            Self::UnsupportedHistoryPolicy(_) => "dump-history-policy",
            Self::WholeEditionCollectionRequired => "dump-whole-edition-scope",
            Self::WholeEditionBootstrapRequired => "dump-whole-edition-bootstrap-required",
            Self::WholeEditionLongGap { .. } => "dump-whole-edition-long-gap",
            Self::InvalidSnapshotTimestamp | Self::InvalidRaceWindowTimestamp => {
                "dump-race-window-timestamp"
            }
            Self::RaceWindowPrecedesSnapshot => "dump-race-window-order",
            Self::InvalidRecentChangesContinuation => "dump-recent-changes-cursor",
            Self::InvalidRecentChangeTimestamp(_) => "dump-recent-changes-timestamp",
            Self::InvalidRecentChangeTitle(_) => "dump-recent-changes-title",
            Self::DatabaseMismatch { .. } => "dump-database-mismatch",
            Self::LanguageMismatch { .. } => "dump-language-mismatch",
            Self::MissingMainNamespace | Self::InvalidMainNamespace => "dump-namespace-mismatch",
            Self::MissingBaseUrl | Self::InvalidSourceUrl | Self::SourceOriginMismatch => {
                "dump-source-mismatch"
            }
            Self::ClientEndpointMismatch => "dump-client-source-mismatch",
            Self::InvalidPageNamespace { .. } => "dump-page-namespace",
            Self::UnsupportedContentFormat { .. } => "dump-content-format",
            Self::SuppressedRevision(_) => "dump-suppressed-revision",
            Self::ResumeCursorBeyondDump { .. } => "dump-resume-cursor",
            Self::CompletedCursorMismatch { .. } => "dump-completed-cursor",
            Self::DuplicateSelectedPage(_) => "dump-duplicate-page",
            Self::ClosurePageIdentityMismatch { .. } => "dump-closure-page-identity",
            Self::InvalidClosureJob => "dump-closure-job",
        }
    }
}

impl fmt::Display for DumpBootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acquisition(error) => error.fmt(formatter),
            Self::Dump(error) => error.fmt(formatter),
            Self::Capture(error) => error.fmt(formatter),
            Self::Source(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::EmptyDumpSet => formatter.write_str("authenticated dump set is empty"),
            Self::ArtifactLengthOverflow => {
                formatter.write_str("authenticated dump-set length overflows the durable limit")
            }
            Self::PageCursorOverflow => {
                formatter.write_str("multipart dump page cursor overflowed")
            }
            Self::DecompressedByteCounterOverflow => {
                formatter.write_str("multipart dump decompressed-byte counter overflowed")
            }
            Self::UnsupportedHistoryPolicy(policy) => write!(
                formatter,
                "current dump bootstrap requires current-and-future history, not {policy:?}"
            ),
            Self::WholeEditionCollectionRequired => formatter.write_str(
                "whole-edition dump streaming requires a whole-main-namespace collection",
            ),
            Self::WholeEditionBootstrapRequired => formatter.write_str(
                "whole-edition updates require a completed authenticated dump bootstrap",
            ),
            Self::WholeEditionLongGap {
                last_safe_checkpoint,
                source_now,
            } => write!(
                formatter,
                "whole-edition checkpoint {last_safe_checkpoint} is too old for safe RecentChanges recovery at {source_now}; a fresh authenticated dump is required"
            ),
            Self::InvalidSnapshotTimestamp => {
                formatter.write_str("authenticated dump snapshot timestamp is invalid")
            }
            Self::InvalidRaceWindowTimestamp => {
                formatter.write_str("source race-window timestamp is invalid")
            }
            Self::RaceWindowPrecedesSnapshot => {
                formatter.write_str("source race-window boundary precedes the dump snapshot")
            }
            Self::InvalidRecentChangesContinuation => {
                formatter.write_str("durable RecentChanges continuation is invalid")
            }
            Self::InvalidRecentChangeTimestamp(change_id) => write!(
                formatter,
                "RecentChanges row {change_id} has an invalid source timestamp"
            ),
            Self::InvalidRecentChangeTitle(change_id) => write!(
                formatter,
                "RecentChanges row {change_id} has an invalid page title"
            ),
            Self::DatabaseMismatch { expected, actual } => write!(
                formatter,
                "dump database {actual:?} does not match authenticated database {expected:?}"
            ),
            Self::LanguageMismatch { expected, actual } => write!(
                formatter,
                "dump language {actual:?} does not match configured language {expected:?}"
            ),
            Self::MissingMainNamespace => formatter.write_str("dump omitted its main namespace"),
            Self::InvalidMainNamespace => {
                formatter.write_str("dump main namespace metadata is inconsistent")
            }
            Self::MissingBaseUrl => formatter.write_str("dump omitted its canonical base URL"),
            Self::InvalidSourceUrl => formatter.write_str("dump or API source URL is invalid"),
            Self::SourceOriginMismatch => {
                formatter.write_str("dump base URL does not match the configured API origin")
            }
            Self::ClientEndpointMismatch => formatter
                .write_str("Action API client does not match the collection's configured source"),
            Self::InvalidPageNamespace { page_id, namespace } => {
                write!(
                    formatter,
                    "dump page {page_id} used unexpected namespace {namespace}"
                )
            }
            Self::UnsupportedContentFormat {
                revision_id,
                format,
            } => write!(
                formatter,
                "dump revision {revision_id} used unsupported content format {format:?}"
            ),
            Self::SuppressedRevision(revision_id) => {
                write!(
                    formatter,
                    "dump revision {revision_id} has no public source"
                )
            }
            Self::ResumeCursorBeyondDump { stored, actual } => write!(
                formatter,
                "dump resume cursor {stored} exceeds the authenticated dump's {actual} pages"
            ),
            Self::CompletedCursorMismatch { stored, actual } => write!(
                formatter,
                "completed dump cursor {stored} does not match the authenticated dump's {actual} pages"
            ),
            Self::DuplicateSelectedPage(page_id) => {
                write!(formatter, "multipart dump repeated selected page {page_id}")
            }
            Self::ClosurePageIdentityMismatch { expected, actual } => write!(
                formatter,
                "Action API returned page {actual} while closing selected page {expected}"
            ),
            Self::InvalidClosureJob => {
                formatter.write_str("durable dump race-closure job is invalid")
            }
        }
    }
}

impl Error for DumpBootstrapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Acquisition(error) => Some(error),
            Self::Dump(error) => Some(error),
            Self::Capture(error) => Some(error),
            Self::Source(error) => Some(error),
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DumpError> for DumpBootstrapError {
    fn from(error: DumpError) -> Self {
        Self::Dump(error)
    }
}

impl From<DumpAcquisitionError> for DumpBootstrapError {
    fn from(error: DumpAcquisitionError) -> Self {
        Self::Acquisition(error)
    }
}

impl From<CaptureError> for DumpBootstrapError {
    fn from(error: CaptureError) -> Self {
        Self::Capture(error)
    }
}

impl From<ClientError> for DumpBootstrapError {
    fn from(error: ClientError) -> Self {
        Self::Source(error)
    }
}

impl From<StoreError> for DumpBootstrapError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<wikisync_search::SearchError> for DumpBootstrapError {
    fn from(error: wikisync_search::SearchError) -> Self {
        Self::Capture(CaptureError::Search(error))
    }
}

#[cfg(test)]
mod tests {
    use wikisync_core::{
        CollectionBudget, CollectionRemovalPolicy, CollectionRule, InclusionReason, PageTitle,
        RevisionId, TitleSelection,
    };
    use wikisync_mediawiki::{DumpRevision, RevisionMetadata};
    use wikisync_store::{CollectionPreviewCommit, ResolvedCollectionMember};

    use super::*;

    #[test]
    fn canonical_byte_budget_is_checked_before_dump_content_becomes_durable() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.example.invalid/w/api.php", "en")
            .expect("wiki");
        let title = PageTitle::new("Alpha").unwrap();
        let page_id = PageId::new(10).unwrap();
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([title.clone()]).expect("selection"),
        );
        let member = ResolvedCollectionMember {
            page_id,
            namespace: MAIN_NAMESPACE,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title.clone()),
        };
        let (collection_id, _) = library
            .create_collection_from_preview(
                wiki_id,
                "Budgeted dump",
                CollectionPreviewCommit {
                    rule: &rule,
                    history_policy: HistoryPolicy::CurrentAndFuture,
                    budget: CollectionBudget::unlimited().with_maximum_bytes(4).unwrap(),
                    removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                    members: std::slice::from_ref(&member),
                    missing_titles: &[],
                    predicted_canonical_bytes: None,
                },
            )
            .expect("collection");
        let revision_id = RevisionId::new(100).unwrap();
        let page = DumpPage {
            page_id,
            namespace: MAIN_NAMESPACE,
            title,
            redirect_title: None,
            revision: DumpRevision {
                metadata: RevisionMetadata {
                    revision_id,
                    parent_id: None,
                    timestamp: "2026-08-23T10:00:00Z".to_owned(),
                    user: None,
                    user_id: None,
                    comment: None,
                    minor: false,
                    size: Some(5),
                    sha1: None,
                    content_model: Some("wikitext".to_owned()),
                },
                content_format: "text/x-wiki".to_owned(),
                source: Some(b"Alpha".to_vec()),
            },
        };

        let error = import_dump_page(&mut library, wiki_id, collection_id, &page)
            .expect_err("hard canonical-byte budget");
        assert!(matches!(
            error,
            DumpBootstrapError::Store(StoreError::CollectionBudgetExceeded {
                resource: "bytes",
                limit: 4,
                estimated: 5,
            })
        ));
        assert!(library.revision(wiki_id, revision_id).unwrap().is_none());
        assert_eq!(library.logical_object_count().unwrap(), 0);
    }
}
