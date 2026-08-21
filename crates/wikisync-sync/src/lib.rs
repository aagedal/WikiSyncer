//! Synchronization planning, checkpoints, reconciliation, and jobs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::str;

use sha1::{Digest, Sha1};
use wikisync_core::{
    CollectionId, MAIN_NAMESPACE, PageId, PageTitle, RevisionId, TitleSelection, WikiId,
};
use wikisync_mediawiki::{
    CategoryMemberKind, ClientError, MediaWikiClient, PageHeadResolution, RevisionMetadata,
    RevisionOrder, TitleResolution,
};
use wikisync_search::{SearchDocument, SearchError, SearchIndex, SqliteSearchIndex};
use wikisync_store::{
    CurrentRevisionCapture, Library, ObjectId, RevisionCapture, StoreError, StoredPage,
    SyncRunKind, SyncRunStatus,
};

/// Default maximum subcategory depth accepted by one category preview.
pub const DEFAULT_MAX_CATEGORY_DEPTH: u16 = 16;

/// Default maximum number of unique categories visited by one preview.
pub const DEFAULT_MAX_PREVIEW_CATEGORIES: usize = 1_000;

/// Default maximum number of unique main-namespace pages returned by one preview.
pub const DEFAULT_MAX_PREVIEW_PAGES: usize = 10_000;

/// Default maximum number of bounded MediaWiki responses consumed by one preview.
pub const DEFAULT_MAX_PREVIEW_BATCHES: usize = 20_000;

/// Resource bounds for recursive category selection preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CategoryPreviewLimits {
    /// Largest requested recursion depth.
    pub max_recursion_depth: u16,
    /// Largest number of unique category nodes, including the root.
    pub max_categories: usize,
    /// Largest number of unique selected pages.
    pub max_pages: usize,
    /// Largest number of source responses consumed across all categories.
    pub max_batches: usize,
}

impl Default for CategoryPreviewLimits {
    fn default() -> Self {
        Self {
            max_recursion_depth: DEFAULT_MAX_CATEGORY_DEPTH,
            max_categories: DEFAULT_MAX_PREVIEW_CATEGORIES,
            max_pages: DEFAULT_MAX_PREVIEW_PAGES,
            max_batches: DEFAULT_MAX_PREVIEW_BATCHES,
        }
    }
}

/// A main-namespace page included in a category preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CategoryPreviewPage {
    /// Stable MediaWiki page identity used for deduplication and later commitment.
    pub page_id: PageId,
    /// MediaWiki namespace; currently always [`MAIN_NAMESPACE`].
    pub namespace: i32,
    /// Current canonical page title.
    pub title: PageTitle,
}

/// One category actually enumerated while resolving a preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewedCategory {
    /// Canonical category title returned or supplied to MediaWiki.
    pub title: PageTitle,
    /// Number of subcategory edges from the root.
    pub depth: u16,
}

/// Complete, non-persistent result of resolving a category rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CategoryPreview {
    /// Root category supplied by the caller.
    pub root: PageTitle,
    /// Requested maximum subcategory depth.
    pub recursion_depth: u16,
    /// Unique main-namespace pages, sorted by title and then stable page ID.
    pub pages: Vec<CategoryPreviewPage>,
    /// Unique categories actually enumerated in breadth-first order.
    pub categories: Vec<PreviewedCategory>,
    /// Number of bounded MediaWiki responses consumed.
    pub batches: usize,
}

/// Resolves a category recursively without changing a library or collection.
///
/// Depth zero enumerates only direct members of `root`. Subcategories are traversal
/// edges rather than selected pages. Pages are restricted to namespace 0 and
/// deduplicated by stable page ID. The result is returned only when enumeration is
/// complete within every configured resource bound.
pub async fn preview_category_selection(
    client: &MediaWikiClient,
    root: &PageTitle,
    recursion_depth: u16,
    limits: CategoryPreviewLimits,
) -> Result<CategoryPreview, CategoryPreviewError> {
    validate_preview_limits(recursion_depth, limits)?;

    let mut queued = VecDeque::from([(root.clone(), 0_u16)]);
    let mut category_titles = BTreeSet::from([root.clone()]);
    let mut category_ids = BTreeSet::new();
    let mut categories = Vec::new();
    let mut pages = BTreeMap::new();
    let mut batches = 0_usize;

    while let Some((category, depth)) = queued.pop_front() {
        categories.push(PreviewedCategory {
            title: category.clone(),
            depth,
        });
        let mut continuation = None;
        loop {
            if batches == limits.max_batches {
                return Err(CategoryPreviewError::BatchLimitExceeded {
                    limit: limits.max_batches,
                });
            }
            let batch = client
                .category_members_batch(&category, continuation.as_ref())
                .await?;
            batches += 1;

            for member in batch.members {
                match member.kind {
                    CategoryMemberKind::Page => {
                        if !pages.contains_key(&member.page_id) && pages.len() == limits.max_pages {
                            return Err(CategoryPreviewError::PageLimitExceeded {
                                limit: limits.max_pages,
                            });
                        }
                        pages.entry(member.page_id).or_insert(CategoryPreviewPage {
                            page_id: member.page_id,
                            namespace: MAIN_NAMESPACE,
                            title: member.title,
                        });
                    }
                    CategoryMemberKind::Subcategory if depth < recursion_depth => {
                        if category_ids.contains(&member.page_id)
                            || category_titles.contains(&member.title)
                        {
                            continue;
                        }
                        if category_titles.len() == limits.max_categories {
                            return Err(CategoryPreviewError::CategoryLimitExceeded {
                                limit: limits.max_categories,
                            });
                        }
                        category_ids.insert(member.page_id);
                        category_titles.insert(member.title.clone());
                        queued.push_back((member.title, depth + 1));
                    }
                    CategoryMemberKind::Subcategory => {}
                }
            }

            if batch.continuation.is_some() && batch.continuation == continuation {
                return Err(CategoryPreviewError::RepeatedContinuation);
            }
            continuation = batch.continuation;
            if continuation.is_none() {
                break;
            }
        }
    }

    let mut pages = pages.into_values().collect::<Vec<_>>();
    pages.sort_by(|left, right| {
        left.title
            .cmp(&right.title)
            .then_with(|| left.page_id.cmp(&right.page_id))
    });
    Ok(CategoryPreview {
        root: root.clone(),
        recursion_depth,
        pages,
        categories,
        batches,
    })
}

fn validate_preview_limits(
    recursion_depth: u16,
    limits: CategoryPreviewLimits,
) -> Result<(), CategoryPreviewError> {
    if recursion_depth > limits.max_recursion_depth {
        return Err(CategoryPreviewError::DepthLimitExceeded {
            requested: recursion_depth,
            limit: limits.max_recursion_depth,
        });
    }
    if limits.max_categories == 0 || limits.max_pages == 0 || limits.max_batches == 0 {
        return Err(CategoryPreviewError::InvalidLimits);
    }
    Ok(())
}

/// A source, protocol, or configured-bound failure during category preview.
#[derive(Debug)]
pub enum CategoryPreviewError {
    /// MediaWiki access failed.
    Source(ClientError),
    /// Requested recursion exceeds the configured safety ceiling.
    DepthLimitExceeded {
        /// Requested number of subcategory edges.
        requested: u16,
        /// Configured maximum.
        limit: u16,
    },
    /// Traversal discovered more unique categories than permitted.
    CategoryLimitExceeded {
        /// Configured maximum, including the root category.
        limit: usize,
    },
    /// Traversal discovered more unique pages than permitted.
    PageLimitExceeded {
        /// Configured maximum.
        limit: usize,
    },
    /// Traversal required more bounded API responses than permitted.
    BatchLimitExceeded {
        /// Configured maximum.
        limit: usize,
    },
    /// The caller supplied a zero category, page, or batch limit.
    InvalidLimits,
    /// MediaWiki returned the same continuation token twice.
    RepeatedContinuation,
}

impl fmt::Display for CategoryPreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::DepthLimitExceeded { requested, limit } => write!(
                formatter,
                "category recursion depth {requested} exceeds the configured limit of {limit}"
            ),
            Self::CategoryLimitExceeded { limit } => write!(
                formatter,
                "category preview exceeded its {limit}-category limit"
            ),
            Self::PageLimitExceeded { limit } => {
                write!(
                    formatter,
                    "category preview exceeded its {limit}-page limit"
                )
            }
            Self::BatchLimitExceeded { limit } => write!(
                formatter,
                "category preview exceeded its {limit}-response limit"
            ),
            Self::InvalidLimits => formatter
                .write_str("category, page, and response limits must all be greater than zero"),
            Self::RepeatedContinuation => formatter.write_str(
                "MediaWiki repeated a category continuation token without making progress",
            ),
        }
    }
}

impl Error for CategoryPreviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ClientError> for CategoryPreviewError {
    fn from(error: ClientError) -> Self {
        Self::Source(error)
    }
}

/// Result of resolving and capturing one explicit-title selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureReport {
    /// Canonical pages resolved by MediaWiki.
    pub pages: Vec<CapturedPage>,
    /// Canonical titles MediaWiki reported as missing.
    pub missing_titles: Vec<PageTitle>,
}

/// One page head made durable during a capture operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedPage {
    /// Stable remote page identity.
    pub page_id: PageId,
    /// Captured current revision identity.
    pub revision_id: RevisionId,
    /// Logical identity of the canonical source bytes.
    pub content_object_id: ObjectId,
    /// Whether this revision was newly attached to the library.
    pub newly_captured: bool,
}

/// Result of walking and capturing a page's complete available public history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryCaptureReport {
    /// Number of bounded MediaWiki revision-list responses consumed.
    pub batches: usize,
    /// Number of revision metadata records enumerated.
    pub revisions_enumerated: usize,
    /// Number of canonical revision bodies newly captured.
    pub revisions_captured: usize,
    /// Number of already durable revisions whose content was not downloaded again.
    pub revisions_reused: usize,
}

/// Result of reconciling the durable heads selected by one collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationReport {
    /// Completed durable run and checkpoint state.
    pub status: SyncRunStatus,
    /// Whether this invocation recovered an interrupted run.
    pub resumed: bool,
    /// Page-head jobs completed by this invocation.
    pub pages_checked: usize,
    /// Pages whose remote head differed from the locally recorded head.
    pub differing_heads: usize,
    /// Selected page IDs currently unavailable from the source.
    pub missing_pages: usize,
    /// Bounded revision-list responses consumed while closing gaps.
    pub revision_batches: usize,
    /// Revision metadata records observed before reaching durable local history.
    pub revisions_enumerated: usize,
    /// Canonical revision bodies newly captured.
    pub revisions_captured: usize,
    /// Already durable revisions encountered in the forward gap.
    pub revisions_reused: usize,
}

/// Default maximum revision-list responses consumed for one page reconciliation.
pub const DEFAULT_MAX_RECONCILIATION_BATCHES_PER_PAGE: usize = 10_000;

/// Default maximum revisions traversed for one page reconciliation.
pub const DEFAULT_MAX_RECONCILIATION_REVISIONS_PER_PAGE: usize = 1_000_000;

/// Explicit resource ceilings for one page in a long-gap reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconciliationLimits {
    /// Maximum bounded metadata responses consumed for one page.
    pub max_batches_per_page: usize,
    /// Maximum revision metadata records after the durable anchor.
    pub max_revisions_per_page: usize,
}

impl Default for ReconciliationLimits {
    fn default() -> Self {
        Self {
            max_batches_per_page: DEFAULT_MAX_RECONCILIATION_BATCHES_PER_PAGE,
            max_revisions_per_page: DEFAULT_MAX_RECONCILIATION_REVISIONS_PER_PAGE,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PageReconciliationReport {
    head_differed: bool,
    missing: bool,
    revision_batches: usize,
    revisions_enumerated: usize,
    revisions_captured: usize,
    revisions_reused: usize,
}

/// Resolves explicit titles and captures each available current revision.
///
/// Canonical bytes are checked against the source's declared size, content model,
/// and public MediaWiki SHA-1 before the store makes the loose object durable and
/// commits its revision reference. Repeating an unchanged capture is idempotent.
pub async fn capture_explicit_titles(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    selection: &TitleSelection,
) -> Result<CaptureReport, CaptureError> {
    let titles = selection.iter().cloned().collect::<Vec<_>>();
    let resolutions = client.resolve_titles(&titles).await?;
    let mut search_index = SqliteSearchIndex::open(library)?;
    let mut report = CaptureReport {
        pages: Vec::with_capacity(resolutions.len()),
        missing_titles: Vec::new(),
    };

    for resolution in resolutions {
        match resolution {
            TitleResolution::Missing { title, namespace } => {
                library.record_missing_title(collection_id, &title, namespace)?;
                report.missing_titles.push(title);
            }
            TitleResolution::Found(page) => {
                let head = page
                    .current_revision
                    .ok_or(CaptureError::MissingCurrentRevision(page.page_id))?;
                let content = client
                    .revision_content(page.page_id, head.revision_id)
                    .await?;
                if content.metadata.revision_id != head.revision_id {
                    return Err(CaptureError::RevisionIdentityChanged {
                        expected: head.revision_id,
                        actual: content.metadata.revision_id,
                    });
                }
                validate_content(&content.metadata, &content.source)?;

                let newly_captured = library
                    .revision(wiki_id, content.metadata.revision_id)?
                    .is_none();
                let stored = library.capture_current_revision(
                    wiki_id,
                    collection_id,
                    &CurrentRevisionCapture {
                        page_id: page.page_id,
                        namespace: page.namespace,
                        title: &page.title,
                        revision_id: content.metadata.revision_id,
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
                            .expect("validated content model"),
                        source: &content.source,
                    },
                )?;
                let source = str::from_utf8(&content.source)
                    .map_err(|_| CaptureError::InvalidUtf8(content.metadata.revision_id))?;
                let search_content = wikisync_content::to_search_content(source);
                let aliases = library
                    .page_titles(wiki_id, page.page_id)?
                    .into_iter()
                    .filter(|title| title != &page.title)
                    .map(PageTitle::into_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                search_index.index_document(&SearchDocument {
                    wiki_id,
                    page_id: page.page_id,
                    revision_id: content.metadata.revision_id,
                    title: &page.title,
                    aliases: &aliases,
                    headings: &search_content.headings,
                    body: &search_content.body,
                    categories: "",
                    captions: "",
                    transformer_version: search_content.transformer_version.as_str(),
                })?;
                report.pages.push(CapturedPage {
                    page_id: page.page_id,
                    revision_id: content.metadata.revision_id,
                    content_object_id: stored.id,
                    newly_captured,
                });
            }
        }
    }

    Ok(report)
}

/// Enumerates and captures all available public revisions for one captured page.
///
/// MediaWiki continuation is consumed one bounded response at a time. Existing local
/// revisions are validated against the enumerated page, parent, and timestamp and do
/// not trigger another content request. Historical inserts never move the stored page
/// head or replace its current search document.
pub async fn capture_revision_history(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    page_id: PageId,
) -> Result<HistoryCaptureReport, CaptureError> {
    if library.page(wiki_id, page_id)?.is_none() {
        return Err(StoreError::PageNotFound { wiki_id, page_id }.into());
    }

    let mut report = HistoryCaptureReport {
        batches: 0,
        revisions_enumerated: 0,
        revisions_captured: 0,
        revisions_reused: 0,
    };
    let mut continuation = None;
    loop {
        let batch = client
            .revision_batch(page_id, RevisionOrder::NewestFirst, continuation.as_ref())
            .await?;
        report.batches += 1;
        report.revisions_enumerated += batch.revisions.len();

        for metadata in batch.revisions {
            if let Some(existing) = library.revision(wiki_id, metadata.revision_id)? {
                if existing.page_id != page_id
                    || existing.parent_id != metadata.parent_id
                    || existing.timestamp != metadata.timestamp
                {
                    return Err(CaptureError::RevisionMetadataConflict {
                        revision_id: metadata.revision_id,
                    });
                }
                report.revisions_reused += 1;
                continue;
            }

            let content = client
                .revision_content(page_id, metadata.revision_id)
                .await?;
            if content.metadata.parent_id != metadata.parent_id
                || content.metadata.timestamp != metadata.timestamp
            {
                return Err(CaptureError::RevisionMetadataConflict {
                    revision_id: metadata.revision_id,
                });
            }
            validate_content(&content.metadata, &content.source)?;
            let model = content
                .metadata
                .content_model
                .as_deref()
                .expect("validated content model");
            library.capture_revision(
                wiki_id,
                page_id,
                &RevisionCapture {
                    revision_id: content.metadata.revision_id,
                    parent_id: content.metadata.parent_id,
                    timestamp: &content.metadata.timestamp,
                    author: content.metadata.user.as_deref(),
                    author_id: content.metadata.user_id,
                    comment: content.metadata.comment.as_deref(),
                    minor: content.metadata.minor,
                    upstream_sha1: content.metadata.sha1.as_deref(),
                    content_model: model,
                    source: &content.source,
                },
            )?;
            report.revisions_captured += 1;
        }

        continuation = batch.continuation;
        if continuation.is_none() {
            break;
        }
    }
    Ok(report)
}

/// Reconciles every currently selected page against its remote head.
///
/// Each page is represented by a durable, idempotent job. A differing head is streamed
/// forward from the newest durable revision, capturing each bounded response before
/// requesting the next. The page head, search document, and reconciliation checkpoint
/// advance only after all canonical content for every job is durable.
pub async fn reconcile_collection_heads(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    checkpoint_candidate: u64,
) -> Result<ReconciliationReport, CaptureError> {
    reconcile_collection_heads_with_limits(
        client,
        library,
        wiki_id,
        collection_id,
        checkpoint_candidate,
        ReconciliationLimits::default(),
    )
    .await
}

/// Reconciles selected heads under explicit per-page traversal ceilings.
pub async fn reconcile_collection_heads_with_limits(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    checkpoint_candidate: u64,
    limits: ReconciliationLimits,
) -> Result<ReconciliationReport, CaptureError> {
    if limits.max_batches_per_page == 0 || limits.max_revisions_per_page == 0 {
        return Err(CaptureError::InvalidReconciliationLimits);
    }
    let started = library.start_or_resume_sync_run(
        wiki_id,
        Some(collection_id),
        SyncRunKind::Reconciliation,
        checkpoint_candidate,
    )?;
    let run_id = started.status.run_id;

    // Enqueue on both new and resumed runs to close the crash window between
    // creating a run and persisting its complete job set.
    for page in library.collection_pages(wiki_id, collection_id)? {
        let key = format!("reconcile-page:{}", page.page_id);
        let subject = page.page_id.to_string();
        library.enqueue_sync_job(run_id, &key, "reconcile-page-head", Some(&subject))?;
    }

    let mut report = ReconciliationReport {
        status: started.status,
        resumed: started.resumed,
        pages_checked: 0,
        differing_heads: 0,
        missing_pages: 0,
        revision_batches: 0,
        revisions_enumerated: 0,
        revisions_captured: 0,
        revisions_reused: 0,
    };
    while let Some(job) = library.claim_next_sync_job(run_id)? {
        let result = async {
            let raw_page_id = job
                .subject
                .as_deref()
                .ok_or(CaptureError::InvalidReconciliationJob)?
                .parse::<u64>()
                .map_err(|_| CaptureError::InvalidReconciliationJob)?;
            let page_id =
                PageId::new(raw_page_id).map_err(|_| CaptureError::InvalidReconciliationJob)?;
            let page = library
                .page(wiki_id, page_id)?
                .ok_or(StoreError::PageNotFound { wiki_id, page_id })?;
            reconcile_page_head(client, library, wiki_id, collection_id, &page, limits).await
        }
        .await;

        match result {
            Ok(page_report) => {
                library.complete_sync_job(job.job_id)?;
                report.pages_checked += 1;
                report.differing_heads += usize::from(page_report.head_differed);
                report.missing_pages += usize::from(page_report.missing);
                report.revision_batches += page_report.revision_batches;
                report.revisions_enumerated += page_report.revisions_enumerated;
                report.revisions_captured += page_report.revisions_captured;
                report.revisions_reused += page_report.revisions_reused;
            }
            Err(error) => {
                let retryable = error.is_retryable();
                library.fail_sync_job(job.job_id, error.code(), &error.to_string(), retryable)?;
                if !retryable {
                    library.cancel_sync_run(run_id)?;
                }
                return Err(error);
            }
        }
    }

    report.status = library.complete_sync_run(run_id, None)?;
    Ok(report)
}

async fn reconcile_page_head(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    stored_page: &StoredPage,
    limits: ReconciliationLimits,
) -> Result<PageReconciliationReport, CaptureError> {
    let page = match client.resolve_page_head(stored_page.page_id).await? {
        PageHeadResolution::Found(page) => page,
        PageHeadResolution::Missing { page_id } => {
            library.mark_page_missing(wiki_id, collection_id, page_id)?;
            return Ok(PageReconciliationReport {
                missing: true,
                ..PageReconciliationReport::default()
            });
        }
    };
    let head = page
        .current_revision
        .ok_or(CaptureError::MissingCurrentRevision(page.page_id))?;
    let mut report = PageReconciliationReport {
        head_differed: stored_page.current_revision_id != Some(head.revision_id),
        ..PageReconciliationReport::default()
    };

    if report.head_differed {
        let durable_tip = library
            .newest_revision_for_page(wiki_id, page.page_id)?
            .ok_or(StoreError::RevisionNotFound(
                stored_page
                    .current_revision_id
                    .ok_or(CaptureError::MissingLocalPageHead(page.page_id))?,
            ))?;
        if durable_tip.revision_id == head.revision_id {
            validate_existing_revision(&durable_tip, page.page_id, &head)?;
        } else {
            stream_forward_gap(
                client,
                library,
                wiki_id,
                page.page_id,
                &head,
                &durable_tip,
                limits,
                &mut report,
            )
            .await?;
        }
    }

    library.reconcile_current_revision(
        wiki_id,
        collection_id,
        page.page_id,
        page.namespace,
        &page.title,
        head.revision_id,
    )?;
    index_stored_current_revision(
        library,
        wiki_id,
        page.page_id,
        &page.title,
        head.revision_id,
    )?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn stream_forward_gap(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    page_id: PageId,
    remote_head: &RevisionMetadata,
    durable_tip: &wikisync_store::StoredRevision,
    limits: ReconciliationLimits,
    report: &mut PageReconciliationReport,
) -> Result<(), CaptureError> {
    let mut continuation = None;
    let mut previous_revision = durable_tip.revision_id;
    let mut observed_anchor = false;
    let mut reached_remote_head = false;
    loop {
        if report.revision_batches == limits.max_batches_per_page {
            return Err(CaptureError::ReconciliationLimitExceeded {
                page_id,
                kind: "metadata batches",
                limit: limits.max_batches_per_page,
            });
        }
        let batch = client
            .revision_batch_from(
                page_id,
                Some(durable_tip.revision_id),
                RevisionOrder::OldestFirst,
                continuation.as_ref(),
            )
            .await?;
        report.revision_batches += 1;
        for metadata in batch.revisions {
            if !observed_anchor {
                observed_anchor = true;
                if metadata.revision_id != durable_tip.revision_id {
                    return Err(CaptureError::RevisionIdentityChanged {
                        expected: durable_tip.revision_id,
                        actual: metadata.revision_id,
                    });
                }
                validate_existing_revision(durable_tip, page_id, &metadata)?;
                continue;
            }
            if metadata.parent_id != Some(previous_revision) {
                return Err(CaptureError::RevisionChainDisconnected {
                    page_id,
                    local_head: durable_tip.revision_id,
                    remote_head: remote_head.revision_id,
                });
            }
            if report.revisions_enumerated == limits.max_revisions_per_page {
                return Err(CaptureError::ReconciliationLimitExceeded {
                    page_id,
                    kind: "revisions",
                    limit: limits.max_revisions_per_page,
                });
            }
            report.revisions_enumerated += 1;
            if let Some(existing) = library.revision(wiki_id, metadata.revision_id)? {
                validate_existing_revision(&existing, page_id, &metadata)?;
                report.revisions_reused += 1;
            } else {
                let content = client
                    .revision_content(page_id, metadata.revision_id)
                    .await?;
                if content.metadata.parent_id != metadata.parent_id
                    || content.metadata.timestamp != metadata.timestamp
                {
                    return Err(CaptureError::RevisionMetadataConflict {
                        revision_id: metadata.revision_id,
                    });
                }
                validate_content(&content.metadata, &content.source)?;
                library.capture_revision(
                    wiki_id,
                    page_id,
                    &revision_capture(&content.metadata, &content.source),
                )?;
                report.revisions_captured += 1;
            }
            previous_revision = metadata.revision_id;
            if metadata.revision_id == remote_head.revision_id {
                if metadata.parent_id != remote_head.parent_id
                    || metadata.timestamp != remote_head.timestamp
                {
                    return Err(CaptureError::RevisionMetadataConflict {
                        revision_id: remote_head.revision_id,
                    });
                }
                reached_remote_head = true;
                break;
            }
        }
        if reached_remote_head {
            break;
        }
        continuation = batch.continuation;
        if continuation.is_none() {
            break;
        }
    }
    if !observed_anchor || !reached_remote_head {
        return Err(CaptureError::RevisionChainDisconnected {
            page_id,
            local_head: durable_tip.revision_id,
            remote_head: remote_head.revision_id,
        });
    }
    Ok(())
}

fn revision_capture<'a>(metadata: &'a RevisionMetadata, source: &'a [u8]) -> RevisionCapture<'a> {
    RevisionCapture {
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
            .expect("validated content model"),
        source,
    }
}

fn validate_existing_revision(
    existing: &wikisync_store::StoredRevision,
    page_id: PageId,
    metadata: &RevisionMetadata,
) -> Result<(), CaptureError> {
    if existing.page_id != page_id
        || existing.parent_id != metadata.parent_id
        || existing.timestamp != metadata.timestamp
    {
        return Err(CaptureError::RevisionMetadataConflict {
            revision_id: metadata.revision_id,
        });
    }
    Ok(())
}

fn index_stored_current_revision(
    library: &Library,
    wiki_id: WikiId,
    page_id: PageId,
    title: &PageTitle,
    revision_id: RevisionId,
) -> Result<(), CaptureError> {
    let revision = library
        .revision(wiki_id, revision_id)?
        .ok_or(StoreError::RevisionNotFound(revision_id))?;
    let bytes = library.read_object(revision.content_object_id)?;
    let source = str::from_utf8(&bytes).map_err(|_| CaptureError::InvalidUtf8(revision_id))?;
    let search_content = wikisync_content::to_search_content(source);
    let aliases = library
        .page_titles(wiki_id, page_id)?
        .into_iter()
        .filter(|alias| alias != title)
        .map(PageTitle::into_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut search_index = SqliteSearchIndex::open(library)?;
    search_index.index_document(&SearchDocument {
        wiki_id,
        page_id,
        revision_id,
        title,
        aliases: &aliases,
        headings: &search_content.headings,
        body: &search_content.body,
        categories: "",
        captions: "",
        transformer_version: search_content.transformer_version.as_str(),
    })?;
    Ok(())
}

fn validate_content(
    metadata: &wikisync_mediawiki::RevisionMetadata,
    source: &[u8],
) -> Result<(), CaptureError> {
    let model = metadata
        .content_model
        .as_deref()
        .ok_or(CaptureError::MissingContentModel(metadata.revision_id))?;
    if model != "wikitext" {
        return Err(CaptureError::UnsupportedContentModel {
            revision_id: metadata.revision_id,
            model: model.to_owned(),
        });
    }

    let actual_size = source.len() as u64;
    if let Some(expected_size) = metadata.size
        && expected_size != actual_size
    {
        return Err(CaptureError::SizeMismatch {
            revision_id: metadata.revision_id,
            expected: expected_size,
            actual: actual_size,
        });
    }

    if let Some(expected) = metadata.sha1.as_deref() {
        if expected.len() != 31
            || !expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
        {
            return Err(CaptureError::InvalidUpstreamSha1(metadata.revision_id));
        }
        let actual = mediawiki_sha1(source);
        if expected != actual {
            return Err(CaptureError::Sha1Mismatch {
                revision_id: metadata.revision_id,
                expected: expected.to_owned(),
                actual,
            });
        }
    }
    Ok(())
}

fn mediawiki_sha1(bytes: &[u8]) -> String {
    const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut number = Sha1::digest(bytes).to_vec();
    let mut digits = Vec::with_capacity(31);
    while number.iter().any(|byte| *byte != 0) {
        let mut remainder = 0_u16;
        for byte in &mut number {
            let value = (remainder << 8) | u16::from(*byte);
            *byte = (value / 36) as u8;
            remainder = value % 36;
        }
        digits.push(BASE36[usize::from(remainder)]);
    }
    digits.resize(31, b'0');
    digits.reverse();
    String::from_utf8(digits).expect("base36 alphabet is UTF-8")
}

/// A source, validation, or persistence failure during current-revision capture.
#[derive(Debug)]
pub enum CaptureError {
    /// MediaWiki access failed.
    Source(ClientError),
    /// Durable local storage failed.
    Store(StoreError),
    /// Rebuildable current-page indexing failed after canonical capture.
    Search(SearchError),
    /// A resolved page did not expose a current public revision.
    MissingCurrentRevision(PageId),
    /// A durable reconciliation job did not contain a valid page identity.
    InvalidReconciliationJob,
    /// The caller supplied a zero reconciliation ceiling.
    InvalidReconciliationLimits,
    /// A selected page did not have a durable local head to reconcile from.
    MissingLocalPageHead(PageId),
    /// A per-page reconciliation safety ceiling was reached after durable progress.
    ReconciliationLimitExceeded {
        /// Page whose gap exceeded the ceiling.
        page_id: PageId,
        /// Kind of resource that reached its ceiling.
        kind: &'static str,
        /// Configured maximum.
        limit: usize,
    },
    /// Public history did not reconnect a new head to the recorded local head.
    RevisionChainDisconnected {
        /// Page whose public history did not reconnect.
        page_id: PageId,
        /// Most recently recorded local head.
        local_head: RevisionId,
        /// Newly observed remote head.
        remote_head: RevisionId,
    },
    /// Canonical wikitext was not valid UTF-8.
    InvalidUtf8(RevisionId),
    /// The exact-content request returned another revision than title resolution.
    RevisionIdentityChanged {
        /// Head revision selected during title resolution.
        expected: RevisionId,
        /// Revision returned with source content.
        actual: RevisionId,
    },
    /// Enumerated immutable identity disagreed with an existing local revision.
    RevisionMetadataConflict {
        /// Conflicting remote revision identity.
        revision_id: RevisionId,
    },
    /// The main slot did not declare a content model.
    MissingContentModel(RevisionId),
    /// This slice captures canonical wikitext only.
    UnsupportedContentModel {
        /// Revision with the unsupported model.
        revision_id: RevisionId,
        /// Source-declared model.
        model: String,
    },
    /// The canonical byte count differed from MediaWiki metadata.
    SizeMismatch {
        /// Revision being validated.
        revision_id: RevisionId,
        /// MediaWiki-declared byte count.
        expected: u64,
        /// Observed canonical byte count.
        actual: u64,
    },
    /// MediaWiki returned a SHA-1 outside its 31-character base-36 format.
    InvalidUpstreamSha1(RevisionId),
    /// The canonical bytes did not reproduce MediaWiki's public SHA-1.
    Sha1Mismatch {
        /// Revision being validated.
        revision_id: RevisionId,
        /// MediaWiki-declared digest.
        expected: String,
        /// Locally calculated digest.
        actual: String,
    },
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Search(error) => error.fmt(formatter),
            Self::MissingCurrentRevision(page_id) => {
                write!(formatter, "page {page_id} has no public current revision")
            }
            Self::InvalidReconciliationJob => {
                formatter.write_str("durable reconciliation job has an invalid page subject")
            }
            Self::InvalidReconciliationLimits => formatter
                .write_str("reconciliation batch and revision limits must be greater than zero"),
            Self::MissingLocalPageHead(page_id) => {
                write!(
                    formatter,
                    "selected page {page_id} has no durable local head"
                )
            }
            Self::ReconciliationLimitExceeded {
                page_id,
                kind,
                limit,
            } => write!(
                formatter,
                "page {page_id} reconciliation reached its {limit} {kind} limit after saving durable progress"
            ),
            Self::RevisionChainDisconnected {
                page_id,
                local_head,
                remote_head,
            } => write!(
                formatter,
                "page {page_id} history from remote head {remote_head} did not reconnect to local head {local_head}"
            ),
            Self::InvalidUtf8(revision_id) => {
                write!(
                    formatter,
                    "revision {revision_id} source is not valid UTF-8"
                )
            }
            Self::RevisionIdentityChanged { expected, actual } => write!(
                formatter,
                "resolved revision {expected}, but source content belonged to {actual}"
            ),
            Self::RevisionMetadataConflict { revision_id } => write!(
                formatter,
                "enumerated metadata for revision {revision_id} conflicts with the captured revision"
            ),
            Self::MissingContentModel(revision_id) => {
                write!(
                    formatter,
                    "revision {revision_id} has no declared content model"
                )
            }
            Self::UnsupportedContentModel { revision_id, model } => write!(
                formatter,
                "revision {revision_id} uses unsupported content model {model}"
            ),
            Self::SizeMismatch {
                revision_id,
                expected,
                actual,
            } => write!(
                formatter,
                "revision {revision_id} declared {expected} bytes but returned {actual}"
            ),
            Self::InvalidUpstreamSha1(revision_id) => write!(
                formatter,
                "revision {revision_id} returned an invalid MediaWiki SHA-1"
            ),
            Self::Sha1Mismatch {
                revision_id,
                expected,
                actual,
            } => write!(
                formatter,
                "revision {revision_id} SHA-1 mismatch: expected {expected}, calculated {actual}"
            ),
        }
    }
}

impl CaptureError {
    fn code(&self) -> &'static str {
        match self {
            Self::Source(_) => "mediawiki-source",
            Self::Store(_) => "local-store",
            Self::Search(_) => "search-index",
            Self::InvalidReconciliationJob => "invalid-reconciliation-job",
            Self::InvalidReconciliationLimits => "invalid-reconciliation-limits",
            Self::MissingLocalPageHead(_) => "missing-local-page-head",
            Self::ReconciliationLimitExceeded { .. } => "reconciliation-limit",
            Self::MissingCurrentRevision(_) => "page-head-unavailable",
            Self::RevisionChainDisconnected { .. } => "revision-chain-disconnected",
            Self::InvalidUtf8(_)
            | Self::RevisionIdentityChanged { .. }
            | Self::RevisionMetadataConflict { .. }
            | Self::MissingContentModel(_)
            | Self::UnsupportedContentModel { .. }
            | Self::SizeMismatch { .. }
            | Self::InvalidUpstreamSha1(_)
            | Self::Sha1Mismatch { .. } => "revision-validation",
        }
    }

    fn is_retryable(&self) -> bool {
        match self {
            Self::Source(error) => error.is_retryable(),
            Self::Store(_) | Self::Search(_) => true,
            _ => false,
        }
    }
}

impl Error for CaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Search(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ClientError> for CaptureError {
    fn from(error: ClientError) -> Self {
        Self::Source(error)
    }
}

impl From<StoreError> for CaptureError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<SearchError> for CaptureError {
    fn from(error: SearchError) -> Self {
        Self::Search(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikisync_mediawiki::RevisionMetadata;

    fn revision_metadata() -> RevisionMetadata {
        RevisionMetadata {
            revision_id: RevisionId::new(7).expect("revision ID"),
            parent_id: None,
            timestamp: "2026-08-20T00:00:00Z".to_owned(),
            user: None,
            user_id: None,
            comment: None,
            minor: false,
            size: Some(42),
            sha1: Some("mz6rzjalvs99ygh9s19aseznld8m1pu".to_owned()),
            content_model: Some("wikitext".to_owned()),
        }
    }

    #[test]
    fn validates_mediawiki_base36_sha1() {
        let source = b"== Rust ==\nA systems programming language.";
        assert_eq!(mediawiki_sha1(source), "mz6rzjalvs99ygh9s19aseznld8m1pu");
        assert!(validate_content(&revision_metadata(), source).is_ok());
    }

    #[test]
    fn rejects_source_size_or_hash_mismatches() {
        let metadata = revision_metadata();
        assert!(matches!(
            validate_content(&metadata, b"changed"),
            Err(CaptureError::SizeMismatch { .. })
        ));

        let mut metadata = revision_metadata();
        metadata.size = Some(7);
        assert!(matches!(
            validate_content(&metadata, b"changed"),
            Err(CaptureError::Sha1Mismatch { .. })
        ));
    }
}
