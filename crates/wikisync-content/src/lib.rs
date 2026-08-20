//! Deterministic representations derived from canonical MediaWiki wikitext.
//!
//! Canonical source remains untouched in the object store. This crate provides
//! versioned, rebuildable reading representations; changing observable output must
//! therefore introduce a new [`TransformerVersion`].

mod diff;
mod inline;
mod render;

pub use diff::{ContentDiff, DiffLine, DiffMode, DiffSpan, DiffTag, diff};

use std::fmt;

/// A derived representation supported by the first content transformer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OutputKind {
    /// Normalized reading text without Markdown formatting.
    PlainText,
    /// Conservative Markdown retaining common document structure and links.
    Markdown,
}

impl OutputKind {
    /// Returns the cache/export identifier for this output kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlainText => "plain-text",
            Self::Markdown => "markdown",
        }
    }

    /// Returns the exact transformer version used for this output kind.
    #[must_use]
    pub const fn transformer_version(self) -> TransformerVersion {
        match self {
            Self::PlainText => PLAIN_TEXT_TRANSFORMER_VERSION,
            Self::Markdown => MARKDOWN_TRANSFORMER_VERSION,
        }
    }
}

impl fmt::Display for OutputKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A stable identifier included in derived-cache keys and exports.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TransformerVersion(&'static str);

impl TransformerVersion {
    const fn new(value: &'static str) -> Self {
        Self(value)
    }

    /// Returns the stable serialized form of this version.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for TransformerVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Version of the canonical-wikitext-to-plain-text transformation.
pub const PLAIN_TEXT_TRANSFORMER_VERSION: TransformerVersion =
    TransformerVersion::new("wikitext-plain-v1");

/// Version of the canonical-wikitext-to-minimal-Markdown transformation.
pub const MARKDOWN_TRANSFORMER_VERSION: TransformerVersion =
    TransformerVersion::new("wikitext-markdown-v1");

/// One deterministic representation and the version that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedContent {
    /// Representation format.
    pub kind: OutputKind,
    /// Transformer version suitable for a persistent cache key.
    pub transformer_version: TransformerVersion,
    /// Normalized output. Non-empty output ends in exactly one line feed.
    pub body: String,
}

/// Rebuildable fields used by the current-page search index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchContent {
    /// Exact transformer version used for both fields.
    pub transformer_version: TransformerVersion,
    /// Normalized article headings, one per line.
    pub headings: String,
    /// Complete normalized reading text.
    pub body: String,
}

/// Derives one rebuildable representation from canonical UTF-8 wikitext.
///
/// The v1 transformer intentionally recognizes a conservative subset: headings,
/// paragraphs, nested lists, indentation, preformatted blocks, simple tables,
/// internal and external links, emphasis, references, selected text-preserving
/// templates, and common HTML entities. Unknown templates become labeled
/// placeholders instead of being expanded or silently interpreted.
#[must_use]
pub fn transform(source: &str, kind: OutputKind) -> DerivedContent {
    DerivedContent {
        kind,
        transformer_version: kind.transformer_version(),
        body: render::render(source, kind),
    }
}

/// Derives normalized reading text from canonical UTF-8 wikitext.
#[must_use]
pub fn to_plain_text(source: &str) -> String {
    transform(source, OutputKind::PlainText).body
}

/// Derives conservative Markdown from canonical UTF-8 wikitext.
#[must_use]
pub fn to_markdown(source: &str) -> String {
    transform(source, OutputKind::Markdown).body
}

/// Derives the separately weighted fields used by full-text search.
#[must_use]
pub fn to_search_content(source: &str) -> SearchContent {
    SearchContent {
        transformer_version: PLAIN_TEXT_TRANSFORMER_VERSION,
        headings: render::search_headings(source),
        body: to_plain_text(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_explicit_and_output_specific() {
        assert_eq!(
            OutputKind::PlainText.transformer_version().as_str(),
            "wikitext-plain-v1"
        );
        assert_eq!(
            OutputKind::Markdown.transformer_version().as_str(),
            "wikitext-markdown-v1"
        );
        assert_ne!(
            OutputKind::PlainText.transformer_version(),
            OutputKind::Markdown.transformer_version()
        );
    }

    #[test]
    fn empty_input_has_no_synthetic_newline() {
        assert_eq!(to_plain_text(" \r\n\t"), "");
        assert_eq!(to_markdown("<!-- metadata only -->"), "");
    }

    #[test]
    fn search_content_separates_normalized_headings() {
        let content = to_search_content("== [[Rust]] ==\nA language.\n=== History ===\nOld.");
        assert_eq!(content.headings, "Rust\nHistory\n");
        assert_eq!(content.transformer_version, PLAIN_TEXT_TRANSFORMER_VERSION);
        assert!(content.body.contains("A language."));
    }
}
