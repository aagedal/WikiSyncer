//! Selection-aware current-page dump bootstrap and its Action API race closure.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use url::Url;
use wikisync_core::{CollectionId, HistoryPolicy, MAIN_NAMESPACE, PageId, WikiId};
use wikisync_mediawiki::{
    DumpAcquisitionError, DumpError, DumpLimits, DumpPage, DumpReader, DumpSiteInfo,
    MediaWikiClient, PageHeadResolution, VerifiedDumpSet,
};
use wikisync_search::SqliteSearchIndex;
use wikisync_store::{
    CurrentRevisionCapture, DumpImportRequest, DumpImportState, DumpImportStatus, Library,
    StoreError, SyncRunKind, SyncRunStatus,
};

use super::{
    CaptureError, CapturedPage, MediaCaptureReport, NeverCancelled, PageReconciliationReport,
    ReconciliationLimits, capture_resolved_page_head, enforce_collection_byte_budget,
    index_stored_current_revision, reconcile_page_head, repair_missing_sync_manifests,
    validate_content,
};

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
            index_stored_current_revision(
                library,
                wiki_id,
                stored.page_id,
                &stored.title,
                revision_id,
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
            Self::Store(_) => false,
            Self::Dump(_)
            | Self::EmptyDumpSet
            | Self::ArtifactLengthOverflow
            | Self::PageCursorOverflow
            | Self::DecompressedByteCounterOverflow
            | Self::UnsupportedHistoryPolicy(_)
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
            Self::Store(_) => "dump-store",
            Self::Dump(_) => "dump-invalid",
            Self::EmptyDumpSet | Self::ArtifactLengthOverflow => "dump-artifact-set",
            Self::PageCursorOverflow => "dump-page-cursor",
            Self::DecompressedByteCounterOverflow => "dump-decompressed-counter",
            Self::UnsupportedHistoryPolicy(_) => "dump-history-policy",
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
