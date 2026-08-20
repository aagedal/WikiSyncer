//! Synchronization planning, checkpoints, reconciliation, and jobs.

use std::error::Error;
use std::fmt;

use sha1::{Digest, Sha1};
use wikisync_core::{CollectionId, PageId, PageTitle, RevisionId, TitleSelection, WikiId};
use wikisync_mediawiki::{ClientError, MediaWikiClient, TitleResolution};
use wikisync_store::{CurrentRevisionCapture, Library, ObjectId, StoreError};

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
    /// A resolved page did not expose a current public revision.
    MissingCurrentRevision(PageId),
    /// The exact-content request returned another revision than title resolution.
    RevisionIdentityChanged {
        /// Head revision selected during title resolution.
        expected: RevisionId,
        /// Revision returned with source content.
        actual: RevisionId,
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
            Self::MissingCurrentRevision(page_id) => {
                write!(formatter, "page {page_id} has no public current revision")
            }
            Self::RevisionIdentityChanged { expected, actual } => write!(
                formatter,
                "resolved revision {expected}, but source content belonged to {actual}"
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
