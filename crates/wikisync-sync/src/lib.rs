//! Synchronization planning, checkpoints, reconciliation, and jobs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::str;
use std::time::{SystemTime, UNIX_EPOCH};

use sha1::{Digest, Sha1};
use wikisync_core::{
    CollectionBudget, CollectionId, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
    ImagePolicy, InclusionReason, InvalidPageTitle, MAIN_NAMESPACE, PageId, PageTitle, RevisionId,
    TitleSelection, WikiId,
};
use wikisync_mediawiki::{
    CategoryMemberKind, ClientError, MediaWikiClient, PageHeadResolution, ResolvedPage,
    RevisionMetadata, RevisionOrder, ThumbnailMetadataResolution,
    ThumbnailMimeType as SourceThumbnailMimeType, TitleResolution,
};
use wikisync_search::{SearchDocument, SearchError, SearchIndex, SqliteSearchIndex};
use wikisync_store::{
    CollectionPreviewCommit, CurrentRevisionCapture, Library, MediaPlacementKind, MembershipCommit,
    ObjectId, ObjectKind, ResolvedCollectionMember, RevisionCapture, RevisionMediaPlacement,
    StoreError, StoredPage, SyncRunKind, SyncRunStatus, ThumbnailCapture,
    ThumbnailMimeType as StoredThumbnailMimeType,
};

/// Default maximum subcategory depth accepted by one category preview.
pub const DEFAULT_MAX_CATEGORY_DEPTH: u16 = 16;

/// Default maximum number of unique categories visited by one preview.
pub const DEFAULT_MAX_PREVIEW_CATEGORIES: usize = 1_000;

/// Default maximum number of unique main-namespace pages returned by one preview.
pub const DEFAULT_MAX_PREVIEW_PAGES: usize = 10_000;

/// Default maximum number of bounded MediaWiki responses consumed by one preview.
pub const DEFAULT_MAX_PREVIEW_BATCHES: usize = 20_000;

/// Default maximum number of unique titles accepted from one newline-delimited import.
pub const DEFAULT_MAX_TITLE_LIST_TITLES: usize = 10_000;

/// Parses a bounded newline-delimited title list without performing I/O.
///
/// Blank lines are ignored, duplicate titles are removed, and a UTF-8 byte-order
/// mark is accepted at the beginning of the first line. The returned selection is
/// ordered deterministically by [`PageTitle`].
pub fn parse_title_list(source: &str, max_titles: usize) -> Result<TitleSelection, TitleListError> {
    if max_titles == 0 {
        return Err(TitleListError::InvalidLimit);
    }
    let mut titles = BTreeSet::new();
    for (index, raw_line) in source.lines().enumerate() {
        let raw_line = if index == 0 {
            raw_line.strip_prefix('\u{feff}').unwrap_or(raw_line)
        } else {
            raw_line
        };
        if raw_line.trim().is_empty() {
            continue;
        }
        let title = PageTitle::new(raw_line).map_err(|source| TitleListError::InvalidTitle {
            line: index + 1,
            source,
        })?;
        titles.insert(title);
        if titles.len() > max_titles {
            return Err(TitleListError::TitleLimitExceeded { limit: max_titles });
        }
    }
    TitleSelection::new(titles).map_err(|_| TitleListError::Empty)
}

/// A validation failure while importing newline-delimited titles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TitleListError {
    /// The caller supplied a zero title ceiling.
    InvalidLimit,
    /// The input contained no non-blank title.
    Empty,
    /// One line was not a valid MediaWiki title.
    InvalidTitle {
        /// One-based input line number.
        line: usize,
        /// Title validation failure.
        source: InvalidPageTitle,
    },
    /// The unique-title ceiling was exceeded.
    TitleLimitExceeded {
        /// Configured maximum unique title count.
        limit: usize,
    },
}

impl fmt::Display for TitleListError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit => formatter.write_str("title-list limit must be greater than zero"),
            Self::Empty => formatter.write_str("title list contains no titles"),
            Self::InvalidTitle { line, source } => {
                write!(formatter, "invalid title on line {line}: {source}")
            }
            Self::TitleLimitExceeded { limit } => {
                write!(formatter, "title list exceeds its {limit}-title limit")
            }
        }
    }
}

impl Error for TitleListError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidTitle { source, .. } => Some(source),
            _ => None,
        }
    }
}

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
    /// Minimum number of subcategory edges from the configured root, determined by
    /// breadth-first traversal.
    pub category_depth: u16,
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

/// A complete, non-mutating collection-rule preview ready for explicit commitment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionSelectionPreview {
    /// Exact rule that was resolved.
    pub rule: CollectionRule,
    /// Stable resolved page identities and auditable inclusion reasons.
    pub members: Vec<ResolvedCollectionMember>,
    /// Fixed titles currently absent from the source.
    pub missing_titles: Vec<PageTitle>,
    /// Sum of source-declared current-revision sizes when every size was available.
    pub predicted_canonical_bytes: Option<u64>,
    /// Number of bounded category-member responses, or zero for title rules.
    pub category_batches: usize,
}

/// Resolves any MVP collection rule without changing the library.
pub async fn preview_collection_rule(
    client: &MediaWikiClient,
    rule: &CollectionRule,
    limits: CategoryPreviewLimits,
) -> Result<CollectionSelectionPreview, CollectionPreviewError> {
    match rule {
        CollectionRule::Category {
            title,
            recursion_depth,
        } => {
            let preview =
                preview_category_selection(client, title, *recursion_depth, limits).await?;
            Ok(CollectionSelectionPreview {
                rule: rule.clone(),
                members: preview
                    .pages
                    .into_iter()
                    .map(|page| ResolvedCollectionMember {
                        page_id: page.page_id,
                        namespace: page.namespace,
                        title: page.title,
                        inclusion_reason: InclusionReason::Category {
                            category: title.clone(),
                            depth: page.category_depth,
                        },
                    })
                    .collect(),
                missing_titles: Vec::new(),
                predicted_canonical_bytes: None,
                category_batches: preview.batches,
            })
        }
        CollectionRule::ExplicitTitles(selection) | CollectionRule::TitleList(selection) => {
            let resolutions = client
                .resolve_titles(&selection.iter().cloned().collect::<Vec<_>>())
                .await?;
            let mut members = Vec::new();
            let mut missing_titles = Vec::new();
            let mut predicted = Some(0_u64);
            for resolution in resolutions {
                match resolution {
                    TitleResolution::Missing { title, .. } => missing_titles.push(title),
                    TitleResolution::Found(page) => {
                        predicted = predicted.and_then(|total| {
                            page.current_revision
                                .as_ref()
                                .and_then(|revision| revision.size)
                                .and_then(|size| total.checked_add(size))
                        });
                        let reason = match rule {
                            CollectionRule::ExplicitTitles(_) => {
                                InclusionReason::ExplicitTitle(page.title.clone())
                            }
                            CollectionRule::TitleList(_) => {
                                InclusionReason::TitleList(page.title.clone())
                            }
                            CollectionRule::Category { .. } => unreachable!("matched title rule"),
                        };
                        members.push(ResolvedCollectionMember {
                            page_id: page.page_id,
                            namespace: page.namespace,
                            title: page.title,
                            inclusion_reason: reason,
                        });
                    }
                }
            }
            members.sort_by_key(|member| member.page_id);
            missing_titles.sort();
            Ok(CollectionSelectionPreview {
                rule: rule.clone(),
                members,
                missing_titles,
                predicted_canonical_bytes: predicted,
                category_batches: 0,
            })
        }
    }
}

/// Commits a completed preview and all collection policy fields.
///
/// A preview that exceeds either hard budget is rejected before membership changes.
pub fn commit_collection_preview(
    library: &mut Library,
    collection_id: CollectionId,
    preview: &CollectionSelectionPreview,
    history_policy: HistoryPolicy,
    budget: CollectionBudget,
    removal_policy: CollectionRemovalPolicy,
) -> Result<MembershipCommit, StoreError> {
    let expected_generation = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?
        .generation;
    library.update_collection_from_preview(
        collection_id,
        expected_generation,
        None,
        CollectionPreviewCommit {
            rule: &preview.rule,
            history_policy,
            budget,
            removal_policy,
            members: &preview.members,
            missing_titles: &preview.missing_titles,
            predicted_canonical_bytes: preview.predicted_canonical_bytes,
        },
    )
}

/// Outcome of periodically re-resolving a collection's dynamic membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DynamicMembershipReconciliation {
    /// Fixed-title rules are not dynamic and were left completely unchanged.
    StaticRule,
    /// A category rule was completely re-resolved and atomically committed.
    Category {
        /// Number of bounded category-member responses consumed by the preview.
        category_batches: usize,
        /// Active and newly removed membership counts from the atomic commit.
        membership: MembershipCommit,
    },
}

/// Re-resolves a configured category rule before synchronizing its active members.
///
/// The bounded category preview completes without mutating the library. Only a
/// successful complete preview that fits the collection's configured budget reaches
/// the store's atomic membership commit, where the configured removal policy is
/// applied. Explicit-title and title-list collections are stable selections and are
/// returned as a no-op without contacting MediaWiki.
pub async fn reconcile_dynamic_collection_membership(
    client: &MediaWikiClient,
    library: &mut Library,
    collection_id: CollectionId,
    limits: CategoryPreviewLimits,
) -> Result<DynamicMembershipReconciliation, DynamicMembershipReconciliationError> {
    let configuration = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
    if !matches!(&configuration.rule, CollectionRule::Category { .. }) {
        return Ok(DynamicMembershipReconciliation::StaticRule);
    }

    let preview = preview_collection_rule(client, &configuration.rule, limits).await?;
    let page_count = u64::try_from(preview.members.len())
        .map_err(|_| StoreError::InvalidConfig("collection preview is too large"))?;
    library.record_collection_estimate(
        collection_id,
        page_count,
        preview.predicted_canonical_bytes,
    )?;
    let membership = library.commit_resolved_membership(collection_id, &preview.members)?;
    Ok(DynamicMembershipReconciliation::Category {
        category_batches: preview.category_batches,
        membership,
    })
}

/// A store or non-mutating preview failure during dynamic membership reconciliation.
#[derive(Debug)]
pub enum DynamicMembershipReconciliationError {
    /// Reading configuration, enforcing budgets, or committing membership failed.
    Store(StoreError),
    /// The bounded category preview did not complete successfully.
    Preview(CollectionPreviewError),
}

impl fmt::Display for DynamicMembershipReconciliationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::Preview(error) => error.fmt(formatter),
        }
    }
}

impl Error for DynamicMembershipReconciliationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Preview(error) => Some(error),
        }
    }
}

impl From<StoreError> for DynamicMembershipReconciliationError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<CollectionPreviewError> for DynamicMembershipReconciliationError {
    fn from(error: CollectionPreviewError) -> Self {
        Self::Preview(error)
    }
}

/// A source or category-preview failure while resolving a collection rule.
#[derive(Debug)]
pub enum CollectionPreviewError {
    /// Title resolution failed.
    Source(ClientError),
    /// Category traversal failed.
    Category(CategoryPreviewError),
}

impl fmt::Display for CollectionPreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Category(error) => error.fmt(formatter),
        }
    }
}

impl Error for CollectionPreviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Category(error) => Some(error),
        }
    }
}

impl From<ClientError> for CollectionPreviewError {
    fn from(error: ClientError) -> Self {
        Self::Source(error)
    }
}

impl From<CategoryPreviewError> for CollectionPreviewError {
    fn from(error: CategoryPreviewError) -> Self {
        Self::Category(error)
    }
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
                            category_depth: depth,
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
    /// Optional thumbnails captured, reused, skipped, or rejected after text commits.
    pub media: MediaCaptureReport,
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
    /// Optional thumbnails processed for selected historical revisions.
    pub media: MediaCaptureReport,
}

/// Bounded outcome of optional media acquisition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaCaptureReport {
    /// Eligible revision placements discovered from the exact revision.
    pub placements_discovered: usize,
    /// Placements newly attached to their durable article revision.
    pub placements_captured: usize,
    /// Already durable placements reused without another media request.
    pub placements_reused: usize,
    /// Placements whose current source metadata was missing or ineligible.
    pub placements_ineligible: usize,
    /// Redacted, bounded failures that did not invalidate canonical text capture.
    pub failures: Vec<MediaCaptureFailure>,
}

impl MediaCaptureReport {
    fn merge(&mut self, other: Self) {
        self.placements_discovered += other.placements_discovered;
        self.placements_captured += other.placements_captured;
        self.placements_reused += other.placements_reused;
        self.placements_ineligible += other.placements_ineligible;
        self.failures.extend(other.failures);
    }
}

/// Stage at which one optional thumbnail could not be captured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaCaptureStage {
    /// Exact-revision image discovery.
    Discovery,
    /// File metadata, attribution, and rendition resolution.
    Metadata,
    /// Bounded thumbnail transfer.
    Download,
    /// Collection budget, raster validation, or durable cataloguing.
    Storage,
}

/// Redacted failure for one optional media operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaCaptureFailure {
    /// Article revision whose optional media was being processed.
    pub revision_id: RevisionId,
    /// Zero-based placement, absent when discovery itself failed.
    pub placement_index: Option<u32>,
    /// Failed acquisition stage.
    pub stage: MediaCaptureStage,
    /// Whether repeating the source operation later may succeed.
    pub retryable: bool,
}

/// Initial capture outcome for one committed collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapReport {
    /// Completed durable bootstrap run and checkpoint state.
    pub status: SyncRunStatus,
    /// Current heads captured for committed members.
    pub current: CaptureReport,
    /// Aggregate historical work required by the configured policy.
    pub history: HistoryCaptureReport,
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
    /// Optional thumbnails processed while reconciling selected revisions.
    pub media: MediaCaptureReport,
}

/// Default maximum revision-list responses consumed for one page reconciliation.
pub const DEFAULT_MAX_RECONCILIATION_BATCHES_PER_PAGE: usize = 10_000;

/// Default maximum revisions traversed for one page reconciliation.
pub const DEFAULT_MAX_RECONCILIATION_REVISIONS_PER_PAGE: usize = 1_000_000;

/// Maximum predecessor-manifest gaps repaired before one sync invocation contacts
/// its source. Larger backlogs are drained across retryable invocations so recovery
/// work remains bounded.
pub const MAX_MANIFEST_REPAIRS_PER_SYNC: u32 = 16;

/// Explicit resource ceilings for one page in a long-gap reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconciliationLimits {
    /// Maximum bounded metadata responses consumed for one page.
    pub max_batches_per_page: usize,
    /// Maximum revision metadata records after the durable anchor.
    pub max_revisions_per_page: usize,
}

/// Cooperative interruption check used at bounded reconciliation boundaries.
///
/// Cancellation is deliberately caller-owned: the synchronization engine observes
/// the probe between durable jobs, requests, batches, revisions, and final metadata
/// advancement. An in-flight bounded request or local transaction is allowed to
/// finish before cancellation is reported.
pub trait CancellationProbe: Sync {
    /// Returns whether the caller has requested that reconciliation stop.
    fn is_cancelled(&self) -> bool;
}

impl<F> CancellationProbe for F
where
    F: Fn() -> bool + Sync,
{
    fn is_cancelled(&self) -> bool {
        self()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct NeverCancelled;

impl CancellationProbe for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

fn check_cancellation(cancellation: &dyn CancellationProbe) -> Result<(), CaptureError> {
    if cancellation.is_cancelled() {
        Err(CaptureError::Cancelled)
    } else {
        Ok(())
    }
}

impl Default for ReconciliationLimits {
    fn default() -> Self {
        Self {
            max_batches_per_page: DEFAULT_MAX_RECONCILIATION_BATCHES_PER_PAGE,
            max_revisions_per_page: DEFAULT_MAX_RECONCILIATION_REVISIONS_PER_PAGE,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PageReconciliationReport {
    head_differed: bool,
    missing: bool,
    revision_batches: usize,
    revisions_enumerated: usize,
    revisions_captured: usize,
    revisions_reused: usize,
    media: MediaCaptureReport,
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
    if library.collection_configuration(collection_id)?.is_none() {
        library.set_collection_configuration(
            collection_id,
            &CollectionRule::ExplicitTitles(selection.clone()),
            HistoryPolicy::CurrentAndFuture,
            CollectionBudget::unlimited(),
            CollectionRemovalPolicy::StopTrackingRetainHistory,
        )?;
    }
    let titles = selection.iter().cloned().collect::<Vec<_>>();
    let resolutions = client.resolve_titles(&titles).await?;
    let mut search_index = SqliteSearchIndex::open(library)?;
    let mut report = CaptureReport {
        pages: Vec::with_capacity(resolutions.len()),
        missing_titles: Vec::new(),
        media: MediaCaptureReport::default(),
    };

    for resolution in resolutions {
        match resolution {
            TitleResolution::Missing { title, namespace } => {
                library.record_missing_title(collection_id, &title, namespace)?;
                report.missing_titles.push(title);
            }
            TitleResolution::Found(page) => {
                let (captured, media) = capture_resolved_page_head(
                    client,
                    library,
                    &mut search_index,
                    wiki_id,
                    collection_id,
                    page,
                )
                .await?;
                report.pages.push(captured);
                report.media.merge(media);
            }
        }
    }

    Ok(report)
}

async fn capture_resolved_page_head(
    client: &MediaWikiClient,
    library: &mut Library,
    search_index: &mut SqliteSearchIndex,
    wiki_id: WikiId,
    collection_id: CollectionId,
    page: ResolvedPage,
) -> Result<(CapturedPage, MediaCaptureReport), CaptureError> {
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
    if newly_captured {
        enforce_collection_byte_budget(library, collection_id, content.source.len() as u64)?;
    }
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
    let image_policy = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?
        .image_policy;
    let media = capture_revision_media(
        client,
        library,
        wiki_id,
        collection_id,
        page.page_id,
        content.metadata.revision_id,
        image_policy,
    )
    .await?;
    Ok((
        CapturedPage {
            page_id: page.page_id,
            revision_id: content.metadata.revision_id,
            content_object_id: stored.id,
            newly_captured,
        },
        media,
    ))
}

fn enforce_collection_byte_budget(
    library: &Library,
    collection_id: CollectionId,
    additional_bytes: u64,
) -> Result<(), StoreError> {
    let Some(configuration) = library.collection_configuration(collection_id)? else {
        return Err(StoreError::CollectionNotConfigured(collection_id));
    };
    let Some(limit) = configuration.budget.maximum_bytes() else {
        return Ok(());
    };
    let current = library
        .collection_estimate(collection_id)?
        .current_canonical_bytes;
    let estimated = current.saturating_add(additional_bytes);
    if estimated > limit.get() {
        return Err(StoreError::CollectionBudgetExceeded {
            resource: "bytes",
            limit: limit.get(),
            estimated,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn capture_revision_media(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    page_id: PageId,
    revision_id: RevisionId,
    image_policy: ImagePolicy,
) -> Result<MediaCaptureReport, CaptureError> {
    let ImagePolicy::Thumbnails(policy) = image_policy else {
        return Ok(MediaCaptureReport::default());
    };
    let mut report = MediaCaptureReport::default();
    let existing = library
        .revision_media(wiki_id, revision_id)?
        .into_iter()
        .map(|media| media.placement_index)
        .collect::<BTreeSet<_>>();
    let placements = match client
        .revision_image_placements(page_id, revision_id, policy)
        .await
    {
        Ok(placements) => placements,
        Err(error) => {
            report.failures.push(MediaCaptureFailure {
                revision_id,
                placement_index: None,
                stage: MediaCaptureStage::Discovery,
                retryable: error.is_retryable(),
            });
            return Ok(report);
        }
    };
    report.placements_discovered = placements.len();
    let captured_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::ClockBeforeUnixEpoch)?
        .as_secs();

    for placement in placements {
        if existing.contains(&placement.index) {
            report.placements_reused += 1;
            continue;
        }
        let metadata = match client.resolve_thumbnail_metadata(&placement, policy).await {
            Ok(ThumbnailMetadataResolution::Eligible(metadata)) => metadata,
            Ok(ThumbnailMetadataResolution::Ineligible(_)) => {
                report.placements_ineligible += 1;
                continue;
            }
            Err(error) => {
                report.failures.push(MediaCaptureFailure {
                    revision_id,
                    placement_index: Some(placement.index),
                    stage: MediaCaptureStage::Metadata,
                    retryable: error.is_retryable(),
                });
                continue;
            }
        };
        let bytes = match client.download_thumbnail(&metadata, policy).await {
            Ok(bytes) => bytes,
            Err(error) => {
                report.failures.push(MediaCaptureFailure {
                    revision_id,
                    placement_index: Some(placement.index),
                    stage: MediaCaptureStage::Download,
                    retryable: error.is_retryable(),
                });
                continue;
            }
        };
        let object_id = ObjectId::for_bytes(ObjectKind::Media, &bytes);
        let additional_bytes = if library.contains(object_id)? {
            0
        } else {
            bytes.len() as u64
        };
        match enforce_collection_byte_budget(library, collection_id, additional_bytes) {
            Ok(()) => {}
            Err(StoreError::CollectionBudgetExceeded { .. }) => {
                report.failures.push(MediaCaptureFailure {
                    revision_id,
                    placement_index: Some(placement.index),
                    stage: MediaCaptureStage::Storage,
                    retryable: false,
                });
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        let mime_type = match metadata.mime_type {
            SourceThumbnailMimeType::Jpeg => StoredThumbnailMimeType::Jpeg,
            SourceThumbnailMimeType::Png => StoredThumbnailMimeType::Png,
        };
        let attribution = metadata.credit.as_deref().unwrap_or(&metadata.artist);
        let capture = ThumbnailCapture {
            media_id: metadata.media_id,
            file_title: &metadata.file_title,
            source_sha1: &metadata.source_sha1,
            original_url: &metadata.thumbnail_url,
            description_url: &metadata.description_url,
            author: &metadata.artist,
            attribution,
            license_name: &metadata.license_short_name,
            license_url: metadata.license_url.as_deref(),
            width: metadata.width,
            height: metadata.height,
            mime_type,
            captured_at,
            source: &bytes,
        };
        let stored = library.capture_revision_thumbnail(
            wiki_id,
            page_id,
            revision_id,
            policy,
            &capture,
            RevisionMediaPlacement {
                index: placement.index,
                kind: MediaPlacementKind::Inline,
                caption: placement.caption.as_deref(),
                alt_text: placement.alt_text.as_deref(),
            },
        );
        match stored {
            Ok(_) => report.placements_captured += 1,
            Err(
                StoreError::InvalidMediaMetadata(_)
                | StoreError::ConflictingMedia(_)
                | StoreError::ConflictingMediaPlacement { .. },
            ) => report.failures.push(MediaCaptureFailure {
                revision_id,
                placement_index: Some(placement.index),
                stage: MediaCaptureStage::Storage,
                retryable: false,
            }),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(report)
}

/// Captures current heads for every committed member by stable page identity.
pub async fn capture_committed_collection(
    client: &MediaWikiClient,
    library: &mut Library,
    collection_id: CollectionId,
) -> Result<CaptureReport, CaptureError> {
    let configuration = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
    let mut search_index = SqliteSearchIndex::open(library)?;
    let mut report = CaptureReport {
        pages: Vec::new(),
        missing_titles: Vec::new(),
        media: MediaCaptureReport::default(),
    };
    for member in library.resolved_collection_members(collection_id)? {
        match client.resolve_page_head(member.page_id).await? {
            PageHeadResolution::Found(page) => {
                let (captured, media) = capture_resolved_page_head(
                    client,
                    library,
                    &mut search_index,
                    configuration.wiki_id,
                    collection_id,
                    *page,
                )
                .await?;
                report.pages.push(captured);
                report.media.merge(media);
            }
            PageHeadResolution::Missing { .. } => report.missing_titles.push(member.title),
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
    capture_revision_history_with_policy(
        client,
        library,
        wiki_id,
        None,
        page_id,
        HistoryPolicy::Complete,
    )
    .await
}

/// Captures the bounded history selected by one collection policy.
pub async fn capture_revision_history_with_policy(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: Option<CollectionId>,
    page_id: PageId,
    policy: HistoryPolicy,
) -> Result<HistoryCaptureReport, CaptureError> {
    if library.page(wiki_id, page_id)?.is_none() {
        return Err(StoreError::PageNotFound { wiki_id, page_id }.into());
    }

    let mut report = HistoryCaptureReport {
        batches: 0,
        revisions_enumerated: 0,
        revisions_captured: 0,
        revisions_reused: 0,
        media: MediaCaptureReport::default(),
    };
    if policy == HistoryPolicy::CurrentAndFuture {
        return Ok(report);
    }
    let mut continuation = None;
    let mut selected = 0_u32;
    let mut complete = false;
    loop {
        let batch = client
            .revision_batch(page_id, RevisionOrder::NewestFirst, continuation.as_ref())
            .await?;
        report.batches += 1;

        for metadata in batch.revisions {
            let include = match policy {
                HistoryPolicy::CurrentAndFuture => false,
                HistoryPolicy::LastN(limit) => selected < limit.get(),
                HistoryPolicy::Since(since) => {
                    parse_mediawiki_timestamp(&metadata.timestamp, metadata.revision_id)?
                        >= since.as_seconds()
                }
                HistoryPolicy::Complete => true,
            };
            if !include {
                complete = true;
                break;
            }
            selected = selected.saturating_add(1);
            report.revisions_enumerated += 1;
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
                if let Some(collection_id) = collection_id {
                    enforce_collection_byte_budget(
                        library,
                        collection_id,
                        content.source.len() as u64,
                    )?;
                }
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
            if let Some(collection_id) = collection_id {
                let image_policy = library
                    .collection_configuration(collection_id)?
                    .ok_or(StoreError::CollectionNotConfigured(collection_id))?
                    .image_policy;
                report.media.merge(
                    capture_revision_media(
                        client,
                        library,
                        wiki_id,
                        collection_id,
                        page_id,
                        metadata.revision_id,
                        image_policy,
                    )
                    .await?,
                );
            }
        }

        if complete {
            break;
        }
        continuation = batch.continuation;
        if continuation.is_none() {
            break;
        }
    }
    Ok(report)
}

/// Captures current heads and configured history for every committed member.
pub async fn bootstrap_collection(
    client: &MediaWikiClient,
    library: &mut Library,
    collection_id: CollectionId,
) -> Result<BootstrapReport, CaptureError> {
    repair_missing_sync_manifests(library)?;
    let configuration = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?;
    let checkpoint_candidate = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::ClockBeforeUnixEpoch)?
        .as_secs();
    let started = library.start_or_resume_sync_run(
        configuration.wiki_id,
        Some(collection_id),
        SyncRunKind::Bootstrap,
        checkpoint_candidate,
    )?;
    let run_id = started.status.run_id;
    for member in library.resolved_collection_members(collection_id)? {
        library.enqueue_sync_job(
            run_id,
            &format!("bootstrap-page:{}", member.page_id),
            "bootstrap-page",
            Some(&member.page_id.to_string()),
        )?;
    }
    let mut current = CaptureReport {
        pages: Vec::new(),
        missing_titles: Vec::new(),
        media: MediaCaptureReport::default(),
    };
    let mut history = HistoryCaptureReport {
        batches: 0,
        revisions_enumerated: 0,
        revisions_captured: 0,
        revisions_reused: 0,
        media: MediaCaptureReport::default(),
    };
    let mut search_index = SqliteSearchIndex::open(library)?;
    while let Some(job) = library.claim_next_sync_job(run_id)? {
        let result: Result<
            (
                Option<CapturedPage>,
                Option<PageTitle>,
                MediaCaptureReport,
                HistoryCaptureReport,
            ),
            CaptureError,
        > = async {
            let page_id = job
                .subject
                .as_deref()
                .ok_or(CaptureError::InvalidBootstrapJob)?
                .parse::<u64>()
                .ok()
                .and_then(|value| PageId::new(value).ok())
                .ok_or(CaptureError::InvalidBootstrapJob)?;
            let member = library
                .resolved_collection_members(collection_id)?
                .into_iter()
                .find(|member| member.page_id == page_id)
                .ok_or(CaptureError::InvalidBootstrapJob)?;
            match client.resolve_page_head(page_id).await? {
                PageHeadResolution::Missing { .. } => Ok((
                    None,
                    Some(member.title),
                    MediaCaptureReport::default(),
                    HistoryCaptureReport {
                        batches: 0,
                        revisions_enumerated: 0,
                        revisions_captured: 0,
                        revisions_reused: 0,
                        media: MediaCaptureReport::default(),
                    },
                )),
                PageHeadResolution::Found(page) => {
                    let (captured, media) = capture_resolved_page_head(
                        client,
                        library,
                        &mut search_index,
                        configuration.wiki_id,
                        collection_id,
                        *page,
                    )
                    .await?;
                    let page_history = capture_revision_history_with_policy(
                        client,
                        library,
                        configuration.wiki_id,
                        Some(collection_id),
                        captured.page_id,
                        configuration.history_policy,
                    )
                    .await?;
                    Ok((Some(captured), None, media, page_history))
                }
            }
        }
        .await;
        match result {
            Ok((captured, missing, current_media, page_history)) => {
                library.complete_sync_job(job.job_id)?;
                current.pages.extend(captured);
                current.missing_titles.extend(missing);
                current.media.merge(current_media);
                history.batches += page_history.batches;
                history.revisions_enumerated += page_history.revisions_enumerated;
                history.revisions_captured += page_history.revisions_captured;
                history.revisions_reused += page_history.revisions_reused;
                history.media.merge(page_history.media);
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
    let status = library.complete_sync_run(run_id, None)?;
    library.append_sync_manifest(status.run_id)?;
    Ok(BootstrapReport {
        status,
        current,
        history,
    })
}

fn parse_mediawiki_timestamp(value: &str, revision_id: RevisionId) -> Result<i64, CaptureError> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return Err(CaptureError::InvalidTimestamp(revision_id));
    }
    let number =
        |start: usize, end: usize| -> Option<i64> { value.get(start..end)?.parse::<i64>().ok() };
    let year = number(0, 4).ok_or(CaptureError::InvalidTimestamp(revision_id))?;
    let month = number(5, 7).ok_or(CaptureError::InvalidTimestamp(revision_id))?;
    let day = number(8, 10).ok_or(CaptureError::InvalidTimestamp(revision_id))?;
    let hour = number(11, 13).ok_or(CaptureError::InvalidTimestamp(revision_id))?;
    let minute = number(14, 16).ok_or(CaptureError::InvalidTimestamp(revision_id))?;
    let second = number(17, 19).ok_or(CaptureError::InvalidTimestamp(revision_id))?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(CaptureError::InvalidTimestamp(revision_id));
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Ok(days * 86_400 + hour * 3_600 + minute * 60 + second)
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
    reconcile_collection_heads_with_limits_and_cancellation(
        client,
        library,
        wiki_id,
        collection_id,
        checkpoint_candidate,
        ReconciliationLimits::default(),
        &NeverCancelled,
    )
    .await
}

/// Reconciles selected heads while cooperatively observing caller cancellation.
///
/// Cancellation leaves the durable run unfinished and resumable. Already completed
/// jobs and canonical revision objects remain durable, while the page head and source
/// checkpoint advance only after their bounded reconciliation work completes.
pub async fn reconcile_collection_heads_with_cancellation(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    checkpoint_candidate: u64,
    cancellation: &dyn CancellationProbe,
) -> Result<ReconciliationReport, CaptureError> {
    reconcile_collection_heads_with_limits_and_cancellation(
        client,
        library,
        wiki_id,
        collection_id,
        checkpoint_candidate,
        ReconciliationLimits::default(),
        cancellation,
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
    reconcile_collection_heads_with_limits_and_cancellation(
        client,
        library,
        wiki_id,
        collection_id,
        checkpoint_candidate,
        limits,
        &NeverCancelled,
    )
    .await
}

/// Reconciles selected heads under explicit limits and cooperative cancellation.
pub async fn reconcile_collection_heads_with_limits_and_cancellation(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    checkpoint_candidate: u64,
    limits: ReconciliationLimits,
    cancellation: &dyn CancellationProbe,
) -> Result<ReconciliationReport, CaptureError> {
    if limits.max_batches_per_page == 0 || limits.max_revisions_per_page == 0 {
        return Err(CaptureError::InvalidReconciliationLimits);
    }
    check_cancellation(cancellation)?;
    repair_missing_sync_manifests(library)?;
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
        check_cancellation(cancellation)?;
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
        media: MediaCaptureReport::default(),
    };
    loop {
        check_cancellation(cancellation)?;
        let Some(job) = library.claim_next_sync_job(run_id)? else {
            break;
        };
        let result = async {
            check_cancellation(cancellation)?;
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
            reconcile_page_head(
                client,
                library,
                wiki_id,
                collection_id,
                &page,
                limits,
                cancellation,
            )
            .await
        }
        .await;

        match result {
            Ok(page_report) => {
                check_cancellation(cancellation)?;
                library.complete_sync_job(job.job_id)?;
                report.pages_checked += 1;
                report.differing_heads += usize::from(page_report.head_differed);
                report.missing_pages += usize::from(page_report.missing);
                report.revision_batches += page_report.revision_batches;
                report.revisions_enumerated += page_report.revisions_enumerated;
                report.revisions_captured += page_report.revisions_captured;
                report.revisions_reused += page_report.revisions_reused;
                report.media.merge(page_report.media);
            }
            Err(CaptureError::Cancelled) => return Err(CaptureError::Cancelled),
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

    check_cancellation(cancellation)?;
    report.status = library.complete_sync_run(run_id, None)?;
    library.append_sync_manifest(report.status.run_id)?;
    Ok(report)
}

fn repair_missing_sync_manifests(library: &mut Library) -> Result<(), CaptureError> {
    let repaired = library.append_missing_sync_manifests(MAX_MANIFEST_REPAIRS_PER_SYNC)?;
    if let Some(next_run_id) = library
        .unmanifested_succeeded_run_ids(1)?
        .into_iter()
        .next()
    {
        return Err(CaptureError::ManifestRepairBacklog {
            repaired: repaired.len(),
            next_run_id,
        });
    }
    Ok(())
}

async fn reconcile_page_head(
    client: &MediaWikiClient,
    library: &mut Library,
    wiki_id: WikiId,
    collection_id: CollectionId,
    stored_page: &StoredPage,
    limits: ReconciliationLimits,
    cancellation: &dyn CancellationProbe,
) -> Result<PageReconciliationReport, CaptureError> {
    check_cancellation(cancellation)?;
    let page = match client.resolve_page_head(stored_page.page_id).await? {
        PageHeadResolution::Found(page) => page,
        PageHeadResolution::Missing { page_id } => {
            check_cancellation(cancellation)?;
            library.mark_page_missing(wiki_id, collection_id, page_id)?;
            return Ok(PageReconciliationReport {
                missing: true,
                ..PageReconciliationReport::default()
            });
        }
    };
    check_cancellation(cancellation)?;
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
                collection_id,
                page.page_id,
                &head,
                &durable_tip,
                limits,
                &mut report,
                cancellation,
            )
            .await?;
        }
    }

    let image_policy = library
        .collection_configuration(collection_id)?
        .ok_or(StoreError::CollectionNotConfigured(collection_id))?
        .image_policy;
    report.media.merge(
        capture_revision_media(
            client,
            library,
            wiki_id,
            collection_id,
            page.page_id,
            head.revision_id,
            image_policy,
        )
        .await?,
    );

    // Head and search-index advancement form one bounded completion boundary.
    // Do not observe cancellation between them and expose a half-updated page.
    check_cancellation(cancellation)?;
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
    collection_id: CollectionId,
    page_id: PageId,
    remote_head: &RevisionMetadata,
    durable_tip: &wikisync_store::StoredRevision,
    limits: ReconciliationLimits,
    report: &mut PageReconciliationReport,
    cancellation: &dyn CancellationProbe,
) -> Result<(), CaptureError> {
    let mut continuation = None;
    let mut previous_revision = durable_tip.revision_id;
    let mut observed_anchor = false;
    let mut reached_remote_head = false;
    loop {
        check_cancellation(cancellation)?;
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
        check_cancellation(cancellation)?;
        report.revision_batches += 1;
        for metadata in batch.revisions {
            check_cancellation(cancellation)?;
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
                check_cancellation(cancellation)?;
                let content = client
                    .revision_content(page_id, metadata.revision_id)
                    .await?;
                check_cancellation(cancellation)?;
                if content.metadata.parent_id != metadata.parent_id
                    || content.metadata.timestamp != metadata.timestamp
                {
                    return Err(CaptureError::RevisionMetadataConflict {
                        revision_id: metadata.revision_id,
                    });
                }
                validate_content(&content.metadata, &content.source)?;
                enforce_collection_byte_budget(
                    library,
                    collection_id,
                    content.source.len() as u64,
                )?;
                library.capture_revision(
                    wiki_id,
                    page_id,
                    &revision_capture(&content.metadata, &content.source),
                )?;
                report.revisions_captured += 1;
                check_cancellation(cancellation)?;
            }
            let image_policy = library
                .collection_configuration(collection_id)?
                .ok_or(StoreError::CollectionNotConfigured(collection_id))?
                .image_policy;
            report.media.merge(
                capture_revision_media(
                    client,
                    library,
                    wiki_id,
                    collection_id,
                    page_id,
                    metadata.revision_id,
                    image_policy,
                )
                .await?,
            );
            check_cancellation(cancellation)?;
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
        check_cancellation(cancellation)?;
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
    check_cancellation(cancellation)?;
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
    /// The caller cooperatively interrupted reconciliation at a durable boundary.
    Cancelled,
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
    /// A durable bootstrap job did not contain a committed page identity.
    InvalidBootstrapJob,
    /// The caller supplied a zero reconciliation ceiling.
    InvalidReconciliationLimits,
    /// More completed runs need predecessor manifests than one invocation repairs.
    ManifestRepairBacklog {
        /// Manifests repaired by this invocation before stopping.
        repaired: usize,
        /// Oldest successful run still missing its manifest.
        next_run_id: u64,
    },
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
    /// MediaWiki returned a revision timestamp outside its required UTC format.
    InvalidTimestamp(RevisionId),
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
            Self::Cancelled => formatter.write_str("synchronization was cancelled"),
            Self::Source(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Search(error) => error.fmt(formatter),
            Self::MissingCurrentRevision(page_id) => {
                write!(formatter, "page {page_id} has no public current revision")
            }
            Self::InvalidReconciliationJob => {
                formatter.write_str("durable reconciliation job has an invalid page subject")
            }
            Self::InvalidBootstrapJob => {
                formatter.write_str("durable bootstrap job has an invalid page subject")
            }
            Self::InvalidReconciliationLimits => formatter
                .write_str("reconciliation batch and revision limits must be greater than zero"),
            Self::ManifestRepairBacklog {
                repaired,
                next_run_id,
            } => write!(
                formatter,
                "repaired {repaired} missing sync manifests; retry to continue from successful run {next_run_id} before contacting the source"
            ),
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
            Self::InvalidTimestamp(revision_id) => write!(
                formatter,
                "revision {revision_id} returned an invalid MediaWiki UTC timestamp"
            ),
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
            Self::Cancelled => "cancelled",
            Self::Source(_) => "mediawiki-source",
            Self::Store(_) => "local-store",
            Self::Search(_) => "search-index",
            Self::InvalidReconciliationJob => "invalid-reconciliation-job",
            Self::InvalidBootstrapJob => "invalid-bootstrap-job",
            Self::InvalidReconciliationLimits => "invalid-reconciliation-limits",
            Self::ManifestRepairBacklog { .. } => "manifest-repair-backlog",
            Self::MissingLocalPageHead(_) => "missing-local-page-head",
            Self::ReconciliationLimitExceeded { .. } => "reconciliation-limit",
            Self::MissingCurrentRevision(_) => "page-head-unavailable",
            Self::RevisionChainDisconnected { .. } => "revision-chain-disconnected",
            Self::InvalidUtf8(_)
            | Self::InvalidTimestamp(_)
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
            Self::Cancelled => false,
            Self::Source(error) => error.is_retryable(),
            Self::Store(_) | Self::Search(_) | Self::ManifestRepairBacklog { .. } => true,
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
    fn title_list_import_is_bounded_deduplicated_and_line_aware() {
        let selection =
            parse_title_list("\u{feff}Rust\r\n\n Ferris \nRust\n", 2).expect("bounded title list");
        assert_eq!(
            selection.iter().map(PageTitle::as_str).collect::<Vec<_>>(),
            ["Ferris", "Rust"]
        );
        assert_eq!(
            parse_title_list("Rust\nFerris\nCargo\n", 2),
            Err(TitleListError::TitleLimitExceeded { limit: 2 })
        );
        assert!(matches!(
            parse_title_list("Rust\nunsafe\ttitle\n", 10),
            Err(TitleListError::InvalidTitle { line: 2, .. })
        ));
        assert_eq!(parse_title_list("\n \n", 10), Err(TitleListError::Empty));
        assert_eq!(
            parse_title_list("Rust", 0),
            Err(TitleListError::InvalidLimit)
        );
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

    #[test]
    fn mediawiki_timestamps_map_to_unix_seconds() {
        let revision_id = RevisionId::new(1).unwrap();
        assert_eq!(
            parse_mediawiki_timestamp("1970-01-01T00:00:00Z", revision_id).unwrap(),
            0
        );
        assert_eq!(
            parse_mediawiki_timestamp("2024-02-29T12:34:56Z", revision_id).unwrap(),
            1_709_210_096
        );
        assert!(matches!(
            parse_mediawiki_timestamp("not-a-timestamp", revision_id),
            Err(CaptureError::InvalidTimestamp(_))
        ));
    }
}
