//! Synchronization planning, checkpoints, reconciliation, and jobs.

use std::error::Error;
use std::fmt;
use std::str;

use sha1::{Digest, Sha1};
use wikisync_core::{CollectionId, PageId, PageTitle, RevisionId, TitleSelection, WikiId};
use wikisync_mediawiki::{ClientError, MediaWikiClient, RevisionOrder, TitleResolution};
use wikisync_search::{SearchDocument, SearchError, SearchIndex, SqliteSearchIndex};
use wikisync_store::{CurrentRevisionCapture, Library, ObjectId, RevisionCapture, StoreError};

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
