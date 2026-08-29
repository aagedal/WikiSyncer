//! Bounded collection-administration DTOs shared by direct and daemon writers.

use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::num::NonZeroU64;

use wikisync_core::{
    CollectionBudget, CollectionId, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
    ImagePolicy, InclusionReason, PageId, PageTitle, ThumbnailPolicy, TitleSelection,
    UnixTimestamp, WikiId,
};
use wikisync_store::{CollectionPreviewCommit, Library, ResolvedCollectionMember};
use wikisync_sync::CollectionSelectionPreview;

use crate::{MAX_COLLECTION_DRAFT_BYTES, OperationError};

const DRAFT_MAGIC: &[u8; 4] = b"WKCD";
const LEGACY_DRAFT_VERSION: u16 = 1;
const DRAFT_VERSION: u16 = 2;
const MAX_COLLECTION_NAME_BYTES: usize = 4 * 1024;
const MAX_TITLE_BYTES: usize = 4 * 1024;
const MAX_PREVIEW_MEMBERS: usize = 10_000;
const MAX_PREVIEW_MISSING_TITLES: usize = 10_000;
const MAX_RULE_TITLES: usize = 10_000;
const MAX_TOTAL_TITLE_FIELDS: usize = 40_000;
const MAX_CATEGORY_BATCHES: usize = 20_000;

/// A fully resolved, non-persistent collection draft ready for estimation or commit.
///
/// Callers resolve the preview before asking either a direct writer or the daemon to
/// commit it. The administration path performs no network access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionDraft {
    /// Existing configured source used to resolve the preview.
    pub wiki_id: WikiId,
    /// User-visible collection name.
    pub name: String,
    /// Complete bounded rule preview, including missing fixed titles.
    pub preview: CollectionSelectionPreview,
    /// Public revision history retained for every selected page.
    pub history_policy: HistoryPolicy,
    /// Hard page and canonical-byte limits.
    pub budget: CollectionBudget,
    /// Non-destructive behavior when dynamic membership changes.
    pub removal_policy: CollectionRemovalPolicy,
}

/// One high-level collection operation shared by direct and daemon writer paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectionAdministration {
    /// Validates and estimates a complete preview without changing the library.
    Estimate(CollectionDraft),
    /// Creates one active collection from a complete preview.
    Add(CollectionDraft),
    /// Creates one active collection with an explicit bounded image policy.
    AddWithImagePolicy {
        /// Complete collection preview and non-image policies.
        draft: CollectionDraft,
        /// Optional bounded image acquisition policy.
        image_policy: ImagePolicy,
    },
    /// Replaces one active collection from a complete preview.
    Edit {
        /// Durable collection identity retained by the edit.
        collection_id: CollectionId,
        /// Configuration generation observed before resolving the replacement preview.
        expected_generation: u64,
        /// Complete replacement configuration and preview.
        draft: CollectionDraft,
    },
    /// Replaces one active collection and its bounded image policy atomically.
    EditWithImagePolicy {
        /// Durable collection identity retained by the edit.
        collection_id: CollectionId,
        /// Configuration generation observed before resolving the replacement preview.
        expected_generation: u64,
        /// Complete replacement configuration and preview.
        draft: CollectionDraft,
        /// Complete replacement image policy.
        image_policy: ImagePolicy,
    },
    /// Tombstones a collection while retaining runs, manifests, and captured data.
    Remove { collection_id: CollectionId },
}

/// Bounded preview estimate returned before collection commitment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollectionDraftEstimate {
    /// Resolved active pages in the preview.
    pub resolved_page_count: u64,
    /// Fixed titles reported missing by the source.
    pub missing_title_count: u64,
    /// Source-declared canonical bytes, when every relevant size was available.
    pub predicted_canonical_bytes: Option<u64>,
    /// Number of bounded category responses used by the preview.
    pub category_batches: u64,
    /// Whether known page/byte estimates satisfy the configured hard limits.
    pub fits_budget: bool,
}

/// Successful result of one high-level collection operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectionAdministrationOutcome {
    /// Non-mutating validation and estimate.
    Estimated(CollectionDraftEstimate),
    /// One collection was durably created.
    Added {
        collection_id: CollectionId,
        estimate: CollectionDraftEstimate,
    },
    /// One collection was durably replaced without changing its identity.
    Edited {
        collection_id: CollectionId,
        estimate: CollectionDraftEstimate,
    },
    /// One collection was tombstoned without purging historical evidence.
    Removed { collection_id: CollectionId },
}

/// Applies collection administration while the caller holds direct writer ownership.
///
/// Daemon dispatch calls this same function, so validation and durable mutation logic
/// cannot diverge between CLI/GUI direct and forwarding paths.
pub fn administer_collection_direct(
    library: &mut Library,
    administration: CollectionAdministration,
) -> Result<CollectionAdministrationOutcome, OperationError> {
    match administration {
        CollectionAdministration::Estimate(draft) => Ok(
            CollectionAdministrationOutcome::Estimated(validated_estimate(&draft)?),
        ),
        CollectionAdministration::Add(draft) => {
            let estimate = validated_estimate(&draft)?;
            if !estimate.fits_budget {
                return Err(OperationError::failed(
                    "collection preview exceeds its configured hard budget",
                ));
            }
            let preview = preview_commit(&draft);
            let (collection_id, _) = library
                .create_collection_from_preview(draft.wiki_id, &draft.name, preview)
                .map_err(operation_failed)?;
            Ok(CollectionAdministrationOutcome::Added {
                collection_id,
                estimate,
            })
        }
        CollectionAdministration::AddWithImagePolicy {
            draft,
            image_policy,
        } => {
            let estimate = validated_estimate(&draft)?;
            if !estimate.fits_budget {
                return Err(OperationError::failed(
                    "collection preview exceeds its configured hard budget",
                ));
            }
            let preview = preview_commit(&draft);
            let (collection_id, _) = library
                .create_collection_from_preview_with_image_policy(
                    draft.wiki_id,
                    &draft.name,
                    preview,
                    image_policy,
                )
                .map_err(operation_failed)?;
            Ok(CollectionAdministrationOutcome::Added {
                collection_id,
                estimate,
            })
        }
        CollectionAdministration::Edit {
            collection_id,
            expected_generation,
            draft,
        } => {
            let estimate = validated_estimate(&draft)?;
            if !estimate.fits_budget {
                return Err(OperationError::failed(
                    "collection preview exceeds its configured hard budget",
                ));
            }
            let configuration = library
                .collection_configuration(collection_id)
                .map_err(operation_failed)?
                .ok_or_else(|| OperationError::failed("collection is not configured"))?;
            if configuration.wiki_id != draft.wiki_id {
                return Err(OperationError::failed(
                    "a collection edit cannot change its source wiki",
                ));
            }
            let preview = preview_commit(&draft);
            library
                .update_collection_from_preview(
                    collection_id,
                    expected_generation,
                    Some(&draft.name),
                    preview,
                )
                .map_err(operation_failed)?;
            Ok(CollectionAdministrationOutcome::Edited {
                collection_id,
                estimate,
            })
        }
        CollectionAdministration::EditWithImagePolicy {
            collection_id,
            expected_generation,
            draft,
            image_policy,
        } => {
            let estimate = validated_estimate(&draft)?;
            if !estimate.fits_budget {
                return Err(OperationError::failed(
                    "collection preview exceeds its configured hard budget",
                ));
            }
            let configuration = library
                .collection_configuration(collection_id)
                .map_err(operation_failed)?
                .ok_or_else(|| OperationError::failed("collection is not configured"))?;
            if configuration.wiki_id != draft.wiki_id {
                return Err(OperationError::failed(
                    "a collection edit cannot change its source wiki",
                ));
            }
            let preview = preview_commit(&draft);
            library
                .update_collection_from_preview_with_image_policy(
                    collection_id,
                    expected_generation,
                    Some(&draft.name),
                    preview,
                    image_policy,
                )
                .map_err(operation_failed)?;
            Ok(CollectionAdministrationOutcome::Edited {
                collection_id,
                estimate,
            })
        }
        CollectionAdministration::Remove { collection_id } => {
            library
                .tombstone_collection(collection_id)
                .map_err(operation_failed)?;
            Ok(CollectionAdministrationOutcome::Removed { collection_id })
        }
    }
}

fn preview_commit(draft: &CollectionDraft) -> CollectionPreviewCommit<'_> {
    CollectionPreviewCommit {
        rule: &draft.preview.rule,
        history_policy: draft.history_policy,
        budget: draft.budget,
        removal_policy: draft.removal_policy,
        members: &draft.preview.members,
        missing_titles: &draft.preview.missing_titles,
        predicted_canonical_bytes: draft.preview.predicted_canonical_bytes,
    }
}

fn validated_estimate(draft: &CollectionDraft) -> Result<CollectionDraftEstimate, OperationError> {
    let estimate = estimate(draft)?;
    let _ = encode_collection_draft(draft)?;
    Ok(estimate)
}

/// Encodes one legacy/default-off draft for bounded chunked transport.
///
/// Image-aware administration uses the internal draft-v2 envelope. Keeping this
/// function on v1 preserves existing callers and makes omission mean default-off.
pub fn encode_collection_draft(draft: &CollectionDraft) -> Result<Vec<u8>, OperationError> {
    encode_collection_draft_version(draft, None)
}

/// Encodes a draft-v2 payload carrying an explicit bounded image policy.
pub(crate) fn encode_collection_draft_with_image_policy(
    draft: &CollectionDraft,
    image_policy: ImagePolicy,
) -> Result<Vec<u8>, OperationError> {
    encode_collection_draft_version(draft, Some(image_policy))
}

fn encode_collection_draft_version(
    draft: &CollectionDraft,
    image_policy: Option<ImagePolicy>,
) -> Result<Vec<u8>, OperationError> {
    let _ = estimate(draft)?;
    let encoded_size = encoded_collection_draft_size(draft, image_policy)?;
    if encoded_size > MAX_COLLECTION_DRAFT_BYTES {
        return Err(OperationError::failed(format!(
            "encoded collection draft is {encoded_size} bytes; maximum is {MAX_COLLECTION_DRAFT_BYTES}"
        )));
    }
    let mut bytes = Vec::with_capacity(encoded_size);
    bytes.extend_from_slice(DRAFT_MAGIC);
    put_u16(
        &mut bytes,
        if image_policy.is_some() {
            DRAFT_VERSION
        } else {
            LEGACY_DRAFT_VERSION
        },
    );
    put_u64(&mut bytes, draft.wiki_id.get());
    put_string(&mut bytes, &draft.name, MAX_COLLECTION_NAME_BYTES)?;
    encode_rule(&mut bytes, &draft.preview.rule)?;
    put_count(&mut bytes, draft.preview.members.len(), MAX_PREVIEW_MEMBERS)?;
    for member in &draft.preview.members {
        put_u64(&mut bytes, member.page_id.get());
        put_i32(&mut bytes, member.namespace);
        put_title(&mut bytes, &member.title)?;
        encode_inclusion_reason(&mut bytes, &member.inclusion_reason)?;
    }
    put_count(
        &mut bytes,
        draft.preview.missing_titles.len(),
        MAX_PREVIEW_MISSING_TITLES,
    )?;
    for title in &draft.preview.missing_titles {
        put_title(&mut bytes, title)?;
    }
    put_optional_u64(&mut bytes, draft.preview.predicted_canonical_bytes);
    put_u64(
        &mut bytes,
        u64::try_from(draft.preview.category_batches)
            .map_err(|_| OperationError::failed("category batch count is too large"))?,
    );
    encode_history_policy(&mut bytes, draft.history_policy);
    put_optional_u64(
        &mut bytes,
        draft.budget.maximum_pages().map(NonZeroU64::get),
    );
    put_optional_u64(
        &mut bytes,
        draft.budget.maximum_bytes().map(NonZeroU64::get),
    );
    put_u8(
        &mut bytes,
        match draft.removal_policy {
            CollectionRemovalPolicy::StopTrackingRetainHistory => 1,
            CollectionRemovalPolicy::KeepTracking => 2,
        },
    );
    if let Some(image_policy) = image_policy {
        encode_image_policy(&mut bytes, image_policy);
    }
    debug_assert_eq!(bytes.len(), encoded_size);
    Ok(bytes)
}

fn encoded_collection_draft_size(
    draft: &CollectionDraft,
    image_policy: Option<ImagePolicy>,
) -> Result<usize, OperationError> {
    let mut size = 4_usize + 2 + 8;
    add_size(&mut size, encoded_string_size(&draft.name)?)?;
    add_size(&mut size, encoded_rule_size(&draft.preview.rule)?)?;
    add_size(&mut size, 4)?;
    for member in &draft.preview.members {
        add_size(&mut size, 8 + 4)?;
        add_size(&mut size, encoded_string_size(member.title.as_str())?)?;
        add_size(
            &mut size,
            match &member.inclusion_reason {
                InclusionReason::WholeMainNamespace => {
                    return Err(OperationError::failed(
                        "whole-edition collections require the dedicated dump bootstrap operation",
                    ));
                }
                InclusionReason::ExplicitTitle(title) | InclusionReason::TitleList(title) => {
                    1_usize
                        .checked_add(encoded_string_size(title.as_str())?)
                        .ok_or_else(|| OperationError::failed("collection draft size overflowed"))?
                }
                InclusionReason::Category { category, .. } => 1_usize
                    .checked_add(encoded_string_size(category.as_str())?)
                    .and_then(|value| value.checked_add(2))
                    .ok_or_else(|| OperationError::failed("collection draft size overflowed"))?,
            },
        )?;
    }
    add_size(&mut size, 4)?;
    for title in &draft.preview.missing_titles {
        add_size(&mut size, encoded_string_size(title.as_str())?)?;
    }
    add_size(
        &mut size,
        if draft.preview.predicted_canonical_bytes.is_some() {
            9
        } else {
            1
        },
    )?;
    add_size(&mut size, 8)?;
    add_size(
        &mut size,
        match draft.history_policy {
            HistoryPolicy::CurrentAndFuture | HistoryPolicy::Complete => 1,
            HistoryPolicy::LastN(_) => 5,
            HistoryPolicy::Since(_) => 9,
        },
    )?;
    add_size(
        &mut size,
        if draft.budget.maximum_pages().is_some() {
            9
        } else {
            1
        },
    )?;
    add_size(
        &mut size,
        if draft.budget.maximum_bytes().is_some() {
            9
        } else {
            1
        },
    )?;
    add_size(&mut size, 1)?;
    if let Some(image_policy) = image_policy {
        add_size(
            &mut size,
            match image_policy {
                ImagePolicy::None => 1,
                ImagePolicy::Thumbnails(_) => 1 + 4 + 4 + 8,
            },
        )?;
    }
    Ok(size)
}

fn encoded_rule_size(rule: &CollectionRule) -> Result<usize, OperationError> {
    match rule {
        CollectionRule::WholeMainNamespace => Err(OperationError::failed(
            "whole-edition collections require the dedicated dump bootstrap operation",
        )),
        CollectionRule::ExplicitTitles(titles) | CollectionRule::TitleList(titles) => {
            let mut size = 1_usize + 4;
            for title in titles.iter() {
                add_size(&mut size, encoded_string_size(title.as_str())?)?;
            }
            Ok(size)
        }
        CollectionRule::Category { title, .. } => 1_usize
            .checked_add(encoded_string_size(title.as_str())?)
            .and_then(|value| value.checked_add(2))
            .ok_or_else(|| OperationError::failed("collection draft size overflowed")),
    }
}

fn encoded_string_size(value: &str) -> Result<usize, OperationError> {
    4_usize
        .checked_add(value.len())
        .ok_or_else(|| OperationError::failed("collection draft size overflowed"))
}

fn add_size(total: &mut usize, additional: usize) -> Result<(), OperationError> {
    *total = total
        .checked_add(additional)
        .ok_or_else(|| OperationError::failed("collection draft size overflowed"))?;
    Ok(())
}

/// Decodes and validates one legacy/default-off staged collection draft.
pub fn decode_collection_draft(bytes: &[u8]) -> Result<CollectionDraft, OperationError> {
    let decoded = decode_collection_draft_version(bytes)?;
    if decoded.image_policy.is_some() {
        return Err(OperationError::failed(
            "image-aware collection drafts require draft-v2 administration",
        ));
    }
    Ok(decoded.draft)
}

#[derive(Debug)]
pub(crate) struct DecodedCollectionDraft {
    pub(crate) draft: CollectionDraft,
    pub(crate) image_policy: Option<ImagePolicy>,
}

pub(crate) fn decode_collection_draft_version(
    bytes: &[u8],
) -> Result<DecodedCollectionDraft, OperationError> {
    if bytes.len() > MAX_COLLECTION_DRAFT_BYTES {
        return Err(OperationError::failed(
            "staged collection draft is too large",
        ));
    }
    let mut decoder = DraftDecoder::new(bytes);
    decoder.magic(DRAFT_MAGIC)?;
    let version = decoder.u16()?;
    if !matches!(version, LEGACY_DRAFT_VERSION | DRAFT_VERSION) {
        return Err(OperationError::failed(
            "unsupported collection draft encoding version",
        ));
    }
    let wiki_id = WikiId::new(decoder.u64()?).map_err(operation_failed)?;
    let name = decoder.string(MAX_COLLECTION_NAME_BYTES)?;
    let rule = decode_rule(&mut decoder)?;
    let member_count = decoder.count(MAX_PREVIEW_MEMBERS)?;
    let mut members = Vec::with_capacity(member_count);
    let mut page_ids = HashSet::with_capacity(member_count);
    for _ in 0..member_count {
        let page_id = PageId::new(decoder.u64()?).map_err(operation_failed)?;
        if !page_ids.insert(page_id) {
            return Err(OperationError::failed(
                "collection preview contains a duplicate page ID",
            ));
        }
        members.push(ResolvedCollectionMember {
            page_id,
            namespace: decoder.i32()?,
            title: decoder.title()?,
            inclusion_reason: decode_inclusion_reason(&mut decoder)?,
        });
    }
    let missing_count = decoder.count(MAX_PREVIEW_MISSING_TITLES)?;
    let mut missing_titles = Vec::with_capacity(missing_count);
    let mut unique_missing = HashSet::with_capacity(missing_count);
    for _ in 0..missing_count {
        let title = decoder.title()?;
        if !unique_missing.insert(title.clone()) {
            return Err(OperationError::failed(
                "collection preview contains a duplicate missing title",
            ));
        }
        missing_titles.push(title);
    }
    let predicted_canonical_bytes = decoder.optional_u64()?;
    let category_batches = usize::try_from(decoder.u64()?)
        .map_err(|_| OperationError::failed("category batch count is too large"))?;
    let history_policy = decode_history_policy(&mut decoder)?;
    let maximum_pages = decoder.optional_u64()?;
    let maximum_bytes = decoder.optional_u64()?;
    let mut budget = CollectionBudget::unlimited();
    if let Some(maximum_pages) = maximum_pages {
        budget = budget
            .with_maximum_pages(maximum_pages)
            .map_err(operation_failed)?;
    }
    if let Some(maximum_bytes) = maximum_bytes {
        budget = budget
            .with_maximum_bytes(maximum_bytes)
            .map_err(operation_failed)?;
    }
    let removal_policy = match decoder.u8()? {
        1 => CollectionRemovalPolicy::StopTrackingRetainHistory,
        2 => CollectionRemovalPolicy::KeepTracking,
        _ => return Err(OperationError::failed("invalid collection removal policy")),
    };
    let image_policy = if version == DRAFT_VERSION {
        Some(decode_image_policy(&mut decoder)?)
    } else {
        None
    };
    decoder.finish()?;
    let draft = CollectionDraft {
        wiki_id,
        name,
        preview: CollectionSelectionPreview {
            rule,
            members,
            missing_titles,
            predicted_canonical_bytes,
            category_batches,
        },
        history_policy,
        budget,
        removal_policy,
    };
    let _ = estimate(&draft)?;
    Ok(DecodedCollectionDraft {
        draft,
        image_policy,
    })
}

fn encode_image_policy(bytes: &mut Vec<u8>, policy: ImagePolicy) {
    match policy {
        ImagePolicy::None => put_u8(bytes, 0),
        ImagePolicy::Thumbnails(policy) => {
            put_u8(bytes, 1);
            put_u32(bytes, policy.maximum_edge_pixels().get());
            put_u32(bytes, policy.maximum_images_per_revision().get());
            put_u64(bytes, policy.maximum_bytes_per_image().get());
        }
    }
}

fn decode_image_policy(decoder: &mut DraftDecoder<'_>) -> Result<ImagePolicy, OperationError> {
    match decoder.u8()? {
        0 => Ok(ImagePolicy::None),
        1 => ThumbnailPolicy::new(decoder.u32()?, decoder.u32()?, decoder.u64()?)
            .map(ImagePolicy::Thumbnails)
            .map_err(operation_failed),
        _ => Err(OperationError::failed("invalid collection image policy")),
    }
}

fn estimate(draft: &CollectionDraft) -> Result<CollectionDraftEstimate, OperationError> {
    if draft.name.trim().is_empty() || draft.name.len() > MAX_COLLECTION_NAME_BYTES {
        return Err(OperationError::failed(
            "collection name must be non-empty and at most 4096 bytes",
        ));
    }
    if draft.preview.members.len() > MAX_PREVIEW_MEMBERS
        || draft.preview.missing_titles.len() > MAX_PREVIEW_MISSING_TITLES
        || draft.preview.category_batches > MAX_CATEGORY_BATCHES
    {
        return Err(OperationError::failed(
            "collection preview exceeds administration bounds",
        ));
    }
    match &draft.preview.rule {
        CollectionRule::ExplicitTitles(_) | CollectionRule::TitleList(_)
            if draft.preview.category_batches != 0 =>
        {
            return Err(OperationError::failed(
                "fixed-title preview cannot report category batches",
            ));
        }
        CollectionRule::Category {
            recursion_depth, ..
        } if *recursion_depth > wikisync_sync::DEFAULT_MAX_CATEGORY_DEPTH => {
            return Err(OperationError::failed(
                "category recursion depth exceeds the preview bound",
            ));
        }
        CollectionRule::Category { .. } if !draft.preview.missing_titles.is_empty() => {
            return Err(OperationError::failed(
                "category preview cannot contain missing fixed titles",
            ));
        }
        _ => {}
    }
    let mut unique_page_ids = HashSet::with_capacity(draft.preview.members.len());
    for member in &draft.preview.members {
        if member.namespace != wikisync_core::MAIN_NAMESPACE {
            return Err(OperationError::failed(
                "collection preview member is outside the main namespace",
            ));
        }
        if !unique_page_ids.insert(member.page_id) {
            return Err(OperationError::failed(
                "collection preview contains a duplicate page ID",
            ));
        }
        if member.title.as_str().len() > MAX_TITLE_BYTES {
            return Err(OperationError::failed(
                "collection preview title is too large",
            ));
        }
        let reason_matches = match (&draft.preview.rule, &member.inclusion_reason) {
            (CollectionRule::ExplicitTitles(_), InclusionReason::ExplicitTitle(title))
            | (CollectionRule::TitleList(_), InclusionReason::TitleList(title)) => {
                title == &member.title
            }
            (
                CollectionRule::Category {
                    title,
                    recursion_depth,
                },
                InclusionReason::Category { category, depth },
            ) => category == title && depth <= recursion_depth,
            _ => false,
        };
        if !reason_matches {
            return Err(OperationError::failed(
                "collection preview inclusion reason does not match its rule",
            ));
        }
    }
    let mut unique_missing = HashSet::with_capacity(draft.preview.missing_titles.len());
    for title in &draft.preview.missing_titles {
        if title.as_str().len() > MAX_TITLE_BYTES || !unique_missing.insert(title) {
            return Err(OperationError::failed(
                "collection preview has an oversized or duplicate missing title",
            ));
        }
    }
    let rule_title_count = draft.preview.rule.titles().map_or(1, TitleSelection::len);
    if rule_title_count > MAX_RULE_TITLES {
        return Err(OperationError::failed(
            "collection rule exceeds the 10000-title limit",
        ));
    }
    let total_title_fields = rule_title_count
        .saturating_add(draft.preview.members.len().saturating_mul(2))
        .saturating_add(draft.preview.missing_titles.len());
    if total_title_fields > MAX_TOTAL_TITLE_FIELDS {
        return Err(OperationError::failed(
            "collection preview has too many title fields",
        ));
    }
    let resolved_page_count = u64::try_from(draft.preview.members.len())
        .map_err(|_| OperationError::failed("collection member count is too large"))?;
    let missing_title_count = u64::try_from(draft.preview.missing_titles.len())
        .map_err(|_| OperationError::failed("missing title count is too large"))?;
    let category_batches = u64::try_from(draft.preview.category_batches)
        .map_err(|_| OperationError::failed("category batch count is too large"))?;
    Ok(CollectionDraftEstimate {
        resolved_page_count,
        missing_title_count,
        predicted_canonical_bytes: draft.preview.predicted_canonical_bytes,
        category_batches,
        fits_budget: draft.budget.permits(
            resolved_page_count,
            draft.preview.predicted_canonical_bytes.unwrap_or_default(),
        ),
    })
}

fn encode_rule(bytes: &mut Vec<u8>, rule: &CollectionRule) -> Result<(), OperationError> {
    match rule {
        CollectionRule::WholeMainNamespace => {
            return Err(OperationError::failed(
                "whole-edition collections require the dedicated dump bootstrap operation",
            ));
        }
        CollectionRule::ExplicitTitles(titles) | CollectionRule::TitleList(titles) => {
            put_u8(
                bytes,
                if matches!(rule, CollectionRule::ExplicitTitles(_)) {
                    1
                } else {
                    2
                },
            );
            put_count(bytes, titles.len(), MAX_RULE_TITLES)?;
            for title in titles.iter() {
                put_title(bytes, title)?;
            }
        }
        CollectionRule::Category {
            title,
            recursion_depth,
        } => {
            put_u8(bytes, 3);
            put_title(bytes, title)?;
            put_u16(bytes, *recursion_depth);
        }
    }
    Ok(())
}

fn decode_rule(decoder: &mut DraftDecoder<'_>) -> Result<CollectionRule, OperationError> {
    match decoder.u8()? {
        tag @ (1 | 2) => {
            let count = decoder.count(MAX_RULE_TITLES)?;
            if count == 0 {
                return Err(OperationError::failed("collection title rule is empty"));
            }
            let mut titles = Vec::with_capacity(count);
            for _ in 0..count {
                titles.push(decoder.title()?);
            }
            let selection = TitleSelection::new(titles).map_err(operation_failed)?;
            if selection.len() != count {
                return Err(OperationError::failed(
                    "collection title rule contains duplicates",
                ));
            }
            Ok(if tag == 1 {
                CollectionRule::ExplicitTitles(selection)
            } else {
                CollectionRule::TitleList(selection)
            })
        }
        3 => Ok(CollectionRule::Category {
            title: decoder.title()?,
            recursion_depth: decoder.u16()?,
        }),
        _ => Err(OperationError::failed("invalid collection rule kind")),
    }
}

fn encode_inclusion_reason(
    bytes: &mut Vec<u8>,
    reason: &InclusionReason,
) -> Result<(), OperationError> {
    match reason {
        InclusionReason::WholeMainNamespace => {
            return Err(OperationError::failed(
                "whole-edition collections require the dedicated dump bootstrap operation",
            ));
        }
        InclusionReason::ExplicitTitle(title) => {
            put_u8(bytes, 1);
            put_title(bytes, title)?;
        }
        InclusionReason::TitleList(title) => {
            put_u8(bytes, 2);
            put_title(bytes, title)?;
        }
        InclusionReason::Category { category, depth } => {
            put_u8(bytes, 3);
            put_title(bytes, category)?;
            put_u16(bytes, *depth);
        }
    }
    Ok(())
}

fn decode_inclusion_reason(
    decoder: &mut DraftDecoder<'_>,
) -> Result<InclusionReason, OperationError> {
    match decoder.u8()? {
        1 => Ok(InclusionReason::ExplicitTitle(decoder.title()?)),
        2 => Ok(InclusionReason::TitleList(decoder.title()?)),
        3 => Ok(InclusionReason::Category {
            category: decoder.title()?,
            depth: decoder.u16()?,
        }),
        _ => Err(OperationError::failed(
            "invalid collection inclusion reason",
        )),
    }
}

fn encode_history_policy(bytes: &mut Vec<u8>, policy: HistoryPolicy) {
    match policy {
        HistoryPolicy::CurrentAndFuture => put_u8(bytes, 1),
        HistoryPolicy::LastN(count) => {
            put_u8(bytes, 2);
            put_u32(bytes, count.get());
        }
        HistoryPolicy::Since(timestamp) => {
            put_u8(bytes, 3);
            put_i64(bytes, timestamp.as_seconds());
        }
        HistoryPolicy::Complete => put_u8(bytes, 4),
    }
}

fn decode_history_policy(decoder: &mut DraftDecoder<'_>) -> Result<HistoryPolicy, OperationError> {
    match decoder.u8()? {
        1 => Ok(HistoryPolicy::CurrentAndFuture),
        2 => HistoryPolicy::last_n(decoder.u32()?).map_err(operation_failed),
        3 => Ok(HistoryPolicy::Since(UnixTimestamp::from_seconds(
            decoder.i64()?,
        ))),
        4 => Ok(HistoryPolicy::Complete),
        _ => Err(OperationError::failed("invalid collection history policy")),
    }
}

fn put_count(bytes: &mut Vec<u8>, count: usize, maximum: usize) -> Result<(), OperationError> {
    if count > maximum {
        return Err(OperationError::failed(
            "collection draft count exceeds its bound",
        ));
    }
    put_u32(
        bytes,
        u32::try_from(count).map_err(|_| OperationError::failed("count is too large"))?,
    );
    Ok(())
}

fn put_title(bytes: &mut Vec<u8>, title: &PageTitle) -> Result<(), OperationError> {
    put_string(bytes, title.as_str(), MAX_TITLE_BYTES)
}

fn put_string(bytes: &mut Vec<u8>, value: &str, maximum: usize) -> Result<(), OperationError> {
    if value.len() > maximum {
        return Err(OperationError::failed(
            "collection draft text field is too large",
        ));
    }
    put_u32(
        bytes,
        u32::try_from(value.len()).map_err(|_| OperationError::failed("text is too large"))?,
    );
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_optional_u64(bytes: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            put_u8(bytes, 1);
            put_u64(bytes, value);
        }
        None => put_u8(bytes, 0),
    }
}

fn put_u8(bytes: &mut Vec<u8>, value: u8) {
    bytes.push(value);
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_i64(bytes: &mut Vec<u8>, value: i64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

#[derive(Debug)]
struct DraftDecoder<'a> {
    cursor: Cursor<&'a [u8]>,
}

impl<'a> DraftDecoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(bytes),
        }
    }

    fn magic(&mut self, expected: &[u8; 4]) -> Result<(), OperationError> {
        let mut actual = [0; 4];
        self.cursor
            .read_exact(&mut actual)
            .map_err(|_| OperationError::failed("truncated collection draft"))?;
        if &actual == expected {
            Ok(())
        } else {
            Err(OperationError::failed("invalid collection draft magic"))
        }
    }

    fn read<const N: usize>(&mut self) -> Result<[u8; N], OperationError> {
        let mut bytes = [0; N];
        self.cursor
            .read_exact(&mut bytes)
            .map_err(|_| OperationError::failed("truncated collection draft"))?;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, OperationError> {
        Ok(self.read::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, OperationError> {
        Ok(u16::from_be_bytes(self.read()?))
    }

    fn u32(&mut self) -> Result<u32, OperationError> {
        Ok(u32::from_be_bytes(self.read()?))
    }

    fn u64(&mut self) -> Result<u64, OperationError> {
        Ok(u64::from_be_bytes(self.read()?))
    }

    fn i32(&mut self) -> Result<i32, OperationError> {
        Ok(i32::from_be_bytes(self.read()?))
    }

    fn i64(&mut self) -> Result<i64, OperationError> {
        Ok(i64::from_be_bytes(self.read()?))
    }

    fn count(&mut self, maximum: usize) -> Result<usize, OperationError> {
        let count = self.u32()? as usize;
        if count > maximum {
            Err(OperationError::failed(
                "collection draft count exceeds its bound",
            ))
        } else {
            Ok(count)
        }
    }

    fn string(&mut self, maximum: usize) -> Result<String, OperationError> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(OperationError::failed(
                "collection draft text field is too large",
            ));
        }
        let mut bytes = vec![0; length];
        self.cursor
            .read_exact(&mut bytes)
            .map_err(|_| OperationError::failed("truncated collection draft text"))?;
        String::from_utf8(bytes)
            .map_err(|_| OperationError::failed("collection draft text is not UTF-8"))
    }

    fn title(&mut self) -> Result<PageTitle, OperationError> {
        PageTitle::new(self.string(MAX_TITLE_BYTES)?).map_err(operation_failed)
    }

    fn optional_u64(&mut self) -> Result<Option<u64>, OperationError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            _ => Err(OperationError::failed("invalid optional integer encoding")),
        }
    }

    fn finish(&self) -> Result<(), OperationError> {
        if self.cursor.position() == self.cursor.get_ref().len() as u64 {
            Ok(())
        } else {
            Err(OperationError::failed(
                "collection draft has trailing bytes",
            ))
        }
    }
}

fn operation_failed(error: impl std::fmt::Display) -> OperationError {
    OperationError::failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use wikisync_store::CollectionStatus;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug)]
    struct TempLibrary(PathBuf);

    impl TempLibrary {
        fn new() -> Self {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_dir()
                .expect("current directory")
                .join(format!(".wsd-collection-{}-{unique}", std::process::id()));
            fs::create_dir(&path).expect("create temporary library");
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

    fn sample_draft(wiki_id: WikiId, name: &str) -> CollectionDraft {
        let rust = PageTitle::new("Rust").expect("title");
        let missing = PageTitle::new("Missing page").expect("title");
        CollectionDraft {
            wiki_id,
            name: name.to_owned(),
            preview: CollectionSelectionPreview {
                rule: CollectionRule::ExplicitTitles(
                    TitleSelection::new([rust.clone(), missing.clone()]).expect("selection"),
                ),
                members: vec![ResolvedCollectionMember {
                    page_id: PageId::new(10).expect("page ID"),
                    namespace: 0,
                    title: rust.clone(),
                    inclusion_reason: InclusionReason::ExplicitTitle(rust),
                }],
                missing_titles: vec![missing],
                predicted_canonical_bytes: Some(1_024),
                category_batches: 0,
            },
            history_policy: HistoryPolicy::last_n(5).expect("history"),
            budget: CollectionBudget::unlimited()
                .with_maximum_pages(10)
                .expect("budget")
                .with_maximum_bytes(10_000)
                .expect("budget"),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
        }
    }

    #[test]
    fn collection_draft_codec_round_trips_every_policy_field() {
        let draft = sample_draft(WikiId::new(7).expect("wiki ID"), "Systems");
        let encoded = encode_collection_draft(&draft).expect("encode");
        assert!(encoded.len() < MAX_COLLECTION_DRAFT_BYTES);
        assert_eq!(&encoded[4..6], &LEGACY_DRAFT_VERSION.to_be_bytes());
        assert_eq!(decode_collection_draft(&encoded).expect("decode"), draft);
        assert_eq!(
            decode_collection_draft_version(&encoded)
                .expect("decode legacy draft")
                .image_policy
                .unwrap_or_default(),
            ImagePolicy::None
        );

        let thumbnails = ThumbnailPolicy::new(640, 8, 1_048_576).expect("thumbnail policy");
        let encoded =
            encode_collection_draft_with_image_policy(&draft, ImagePolicy::Thumbnails(thumbnails))
                .expect("encode image-aware draft");
        assert_eq!(&encoded[4..6], &DRAFT_VERSION.to_be_bytes());
        let decoded = decode_collection_draft_version(&encoded).expect("decode image-aware draft");
        assert_eq!(decoded.draft, draft);
        assert_eq!(
            decoded.image_policy,
            Some(ImagePolicy::Thumbnails(thumbnails))
        );

        let mut invalid =
            encode_collection_draft_with_image_policy(&draft, ImagePolicy::None).expect("encode");
        *invalid.last_mut().expect("policy tag") = 1;
        put_u32(&mut invalid, wikisync_core::MAX_THUMBNAIL_EDGE_PIXELS + 1);
        put_u32(&mut invalid, 1);
        put_u64(&mut invalid, 1);
        assert!(decode_collection_draft_version(&invalid).is_err());
    }

    #[test]
    fn collection_draft_codec_rejects_text_and_member_bounds() {
        let mut long_name = sample_draft(WikiId::new(7).expect("wiki ID"), "Systems");
        long_name.name = "x".repeat(MAX_COLLECTION_NAME_BYTES + 1);
        assert!(encode_collection_draft(&long_name).is_err());

        let mut too_many = sample_draft(WikiId::new(7).expect("wiki ID"), "Systems");
        too_many.preview.members = vec![too_many.preview.members[0].clone(); 10_001];
        assert!(encode_collection_draft(&too_many).is_err());

        let category = PageTitle::new("Category:Bounded").expect("category");
        let long_title = PageTitle::new("x".repeat(MAX_TITLE_BYTES)).expect("long title");
        let mut oversized = sample_draft(WikiId::new(7).expect("wiki ID"), "Oversized");
        oversized.preview.rule = CollectionRule::Category {
            title: category.clone(),
            recursion_depth: 1,
        };
        oversized.preview.members = (1..=4_100_u64)
            .map(|page_id| ResolvedCollectionMember {
                page_id: PageId::new(page_id).expect("page ID"),
                namespace: 0,
                title: long_title.clone(),
                inclusion_reason: InclusionReason::Category {
                    category: category.clone(),
                    depth: 1,
                },
            })
            .collect();
        oversized.preview.missing_titles.clear();
        assert!(
            encoded_collection_draft_size(&oversized, None).expect("preflight size")
                > MAX_COLLECTION_DRAFT_BYTES
        );
        assert!(encode_collection_draft(&oversized).is_err());
    }

    #[test]
    fn direct_add_edit_and_remove_share_one_transactional_lifecycle() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("source");
        let thumbnails =
            ThumbnailPolicy::new(800, 12, 2 * 1024 * 1024).expect("bounded thumbnail policy");
        let added = administer_collection_direct(
            &mut library,
            CollectionAdministration::AddWithImagePolicy {
                draft: sample_draft(wiki_id, "Systems"),
                image_policy: ImagePolicy::Thumbnails(thumbnails),
            },
        )
        .expect("add");
        let CollectionAdministrationOutcome::Added {
            collection_id,
            estimate,
        } = added
        else {
            panic!("unexpected add outcome");
        };
        assert_eq!(estimate.resolved_page_count, 1);
        assert_eq!(
            library.unresolved_titles(collection_id).expect("missing"),
            [PageTitle::new("Missing page").unwrap()]
        );
        let initial_generation = library
            .collection(collection_id)
            .expect("collection")
            .expect("created")
            .generation;
        assert_eq!(
            library
                .collection_configuration(collection_id)
                .expect("configuration")
                .expect("configured")
                .image_policy,
            ImagePolicy::Thumbnails(thumbnails)
        );

        let mut edited = sample_draft(wiki_id, "Programming systems");
        edited.history_policy = HistoryPolicy::Complete;
        let outcome = administer_collection_direct(
            &mut library,
            CollectionAdministration::EditWithImagePolicy {
                collection_id,
                expected_generation: initial_generation,
                draft: edited,
                image_policy: ImagePolicy::None,
            },
        )
        .expect("edit");
        assert!(matches!(
            outcome,
            CollectionAdministrationOutcome::Edited {
                collection_id: edited_id,
                ..
            } if edited_id == collection_id
        ));
        let configuration = library
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured");
        assert_eq!(configuration.name, "Programming systems");
        assert_eq!(configuration.history_policy, HistoryPolicy::Complete);
        assert_eq!(configuration.generation, initial_generation + 1);
        assert_eq!(configuration.image_policy, ImagePolicy::None);

        let stale = administer_collection_direct(
            &mut library,
            CollectionAdministration::EditWithImagePolicy {
                collection_id,
                expected_generation: initial_generation,
                draft: sample_draft(wiki_id, "Stale replacement"),
                image_policy: ImagePolicy::Thumbnails(thumbnails),
            },
        )
        .expect_err("stale preview must be rejected");
        assert!(
            stale
                .message()
                .contains("changed while it was being previewed")
        );
        let unchanged = library
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured");
        assert_eq!(unchanged.name, "Programming systems");
        assert_eq!(unchanged.generation, initial_generation + 1);
        assert_eq!(unchanged.image_policy, ImagePolicy::None);

        administer_collection_direct(
            &mut library,
            CollectionAdministration::Remove { collection_id },
        )
        .expect("remove");
        assert!(library.collections().expect("active").is_empty());
        assert_eq!(
            library
                .collection(collection_id)
                .expect("collection")
                .expect("retained")
                .status,
            CollectionStatus::Tombstoned
        );
    }

    #[test]
    fn rejected_edit_leaves_name_and_preview_unchanged() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("source");
        let CollectionAdministrationOutcome::Added { collection_id, .. } =
            administer_collection_direct(
                &mut library,
                CollectionAdministration::Add(sample_draft(wiki_id, "Original")),
            )
            .expect("add")
        else {
            panic!("unexpected add outcome");
        };
        let mut rejected = sample_draft(wiki_id, "Must not persist");
        rejected.budget = CollectionBudget::unlimited()
            .with_maximum_bytes(100)
            .expect("budget");
        let expected_generation = library
            .collection(collection_id)
            .expect("collection")
            .expect("created")
            .generation;
        assert!(
            administer_collection_direct(
                &mut library,
                CollectionAdministration::Edit {
                    collection_id,
                    expected_generation,
                    draft: rejected,
                },
            )
            .is_err()
        );
        assert_eq!(
            library
                .collection(collection_id)
                .expect("collection")
                .expect("retained")
                .name,
            "Original"
        );
    }
}
