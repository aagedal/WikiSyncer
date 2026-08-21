//! Domain identities and policies shared by WikiSyncer interfaces and services.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

/// MediaWiki's main/article namespace, selected by default for offline reading.
pub const MAIN_NAMESPACE: i32 = 0;

/// MediaWiki's category namespace, traversed but not selected as article content.
pub const CATEGORY_NAMESPACE: i32 = 14;

macro_rules! numeric_id {
    ($name:ident, $label:literal) => {
        #[doc = concat!("A validated ", $label, ".")]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            #[doc = concat!("Creates a ", $label, " from its positive integer value.")]
            pub fn new(value: u64) -> Result<Self, InvalidId> {
                NonZeroU64::new(value)
                    .map(Self)
                    .ok_or(InvalidId { kind: $label })
            }

            #[doc = concat!("Returns the numeric value of this ", $label, ".")]
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl TryFrom<u64> for $name {
            type Error = InvalidId;

            fn try_from(value: u64) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

numeric_id!(WikiId, "wiki ID");
numeric_id!(CollectionId, "collection ID");
numeric_id!(PageId, "MediaWiki page ID");
numeric_id!(RevisionId, "MediaWiki revision ID");

/// The error returned when a numeric identity is zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidId {
    kind: &'static str,
}

impl fmt::Display for InvalidId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} must be greater than zero", self.kind)
    }
}

impl Error for InvalidId {}

/// A user-supplied page title before source-specific MediaWiki normalization.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PageTitle(String);

impl PageTitle {
    /// Validates and stores a title, trimming insignificant surrounding whitespace.
    pub fn new(title: impl Into<String>) -> Result<Self, InvalidPageTitle> {
        let title = title.into();
        let title = title.trim();

        if title.is_empty() {
            return Err(InvalidPageTitle::Empty);
        }
        if title.chars().any(char::is_control) {
            return Err(InvalidPageTitle::ContainsControlCharacter);
        }

        Ok(Self(title.to_owned()))
    }

    /// Returns the title as supplied after whitespace validation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the title and returns its owned string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for PageTitle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl TryFrom<String> for PageTitle {
    type Error = InvalidPageTitle;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for PageTitle {
    type Error = InvalidPageTitle;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A validation error for a page title.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidPageTitle {
    /// The title was empty or contained only whitespace.
    Empty,
    /// The title contained a control character and is unsafe to pass through logs or
    /// line-oriented imports.
    ContainsControlCharacter,
}

impl fmt::Display for InvalidPageTitle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("page title cannot be empty"),
            Self::ContainsControlCharacter => {
                formatter.write_str("page title cannot contain control characters")
            }
        }
    }
}

impl Error for InvalidPageTitle {}

/// A non-empty, deterministic set of explicit page titles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleSelection(BTreeSet<PageTitle>);

impl TitleSelection {
    /// Creates a selection, removing duplicate titles.
    pub fn new(titles: impl IntoIterator<Item = PageTitle>) -> Result<Self, EmptyTitleSelection> {
        let titles = titles.into_iter().collect::<BTreeSet<_>>();
        if titles.is_empty() {
            return Err(EmptyTitleSelection);
        }
        Ok(Self(titles))
    }

    /// Iterates over titles in deterministic lexical order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &PageTitle> {
        self.0.iter()
    }

    /// Returns the number of unique titles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether the selection contains no titles.
    ///
    /// A constructed selection is never empty; this method is provided alongside
    /// [`Self::len`] for conventional collection ergonomics.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Parses a newline-delimited title list.
    ///
    /// Blank lines are ignored, surrounding whitespace is trimmed by [`PageTitle`],
    /// and duplicate titles are removed. The returned selection retains deterministic
    /// lexical ordering rather than file ordering.
    pub fn from_newline_delimited(input: &str) -> Result<Self, InvalidTitleList> {
        let mut titles = Vec::new();
        for (index, line) in input.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let title = PageTitle::new(line).map_err(|source| InvalidTitleList::InvalidTitle {
                line: index + 1,
                source,
            })?;
            titles.push(title);
        }
        Self::new(titles).map_err(|_| InvalidTitleList::Empty)
    }
}

/// The error returned when an explicit-title selection has no titles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmptyTitleSelection;

impl fmt::Display for EmptyTitleSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an explicit-title selection requires at least one title")
    }
}

impl Error for EmptyTitleSelection {}

/// A newline-delimited title list could not be converted into a selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidTitleList {
    /// The input contained no non-blank titles.
    Empty,
    /// A non-blank line was not a safe page title.
    InvalidTitle {
        /// One-based line number in the imported text.
        line: usize,
        /// Title validation failure.
        source: InvalidPageTitle,
    },
}

impl fmt::Display for InvalidTitleList {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("title list contains no titles"),
            Self::InvalidTitle { line, source } => {
                write!(formatter, "invalid title on line {line}: {source}")
            }
        }
    }
}

impl Error for InvalidTitleList {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidTitle { source, .. } => Some(source),
            Self::Empty => None,
        }
    }
}

/// The rule used to resolve membership in a collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectionRule {
    /// A fixed set of titles entered directly by the user.
    ExplicitTitles(TitleSelection),
    /// A fixed set of titles imported from a newline-delimited list.
    TitleList(TitleSelection),
    /// Pages reachable from a category, including the category at depth zero.
    Category {
        /// The source category title.
        title: PageTitle,
        /// Maximum number of subcategory edges to traverse.
        recursion_depth: u16,
    },
}

impl CollectionRule {
    /// Returns the fixed titles for a direct or imported selection.
    #[must_use]
    pub fn titles(&self) -> Option<&TitleSelection> {
        match self {
            Self::ExplicitTitles(titles) | Self::TitleList(titles) => Some(titles),
            Self::Category { .. } => None,
        }
    }
}

/// Why one page was included in a resolved collection preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InclusionReason {
    /// The title was entered directly.
    ExplicitTitle(PageTitle),
    /// The title came from a newline-delimited import.
    TitleList(PageTitle),
    /// The page was reached while resolving a category rule.
    Category {
        /// Category from which traversal began.
        category: PageTitle,
        /// Number of subcategory edges between the configured category and the page.
        depth: u16,
    },
}

/// What to do when a dynamic rule no longer resolves a previously selected page.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CollectionRemovalPolicy {
    /// Stop tracking the page while retaining every already captured revision.
    #[default]
    StopTrackingRetainHistory,
    /// Retain the page as an active member until the user removes it explicitly.
    KeepTracking,
}

/// Hard page-count and canonical-storage limits for a collection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CollectionBudget {
    maximum_pages: Option<NonZeroU64>,
    maximum_bytes: Option<NonZeroU64>,
}

impl CollectionBudget {
    /// Creates an unlimited budget.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            maximum_pages: None,
            maximum_bytes: None,
        }
    }

    /// Sets a hard maximum resolved page count.
    pub fn with_maximum_pages(mut self, pages: u64) -> Result<Self, InvalidCollectionBudget> {
        self.maximum_pages =
            Some(NonZeroU64::new(pages).ok_or(InvalidCollectionBudget::ZeroMaximumPages)?);
        Ok(self)
    }

    /// Sets a hard maximum number of canonical storage bytes.
    pub fn with_maximum_bytes(mut self, bytes: u64) -> Result<Self, InvalidCollectionBudget> {
        self.maximum_bytes =
            Some(NonZeroU64::new(bytes).ok_or(InvalidCollectionBudget::ZeroMaximumBytes)?);
        Ok(self)
    }

    /// Returns the hard page limit, or `None` when unlimited.
    #[must_use]
    pub const fn maximum_pages(self) -> Option<NonZeroU64> {
        self.maximum_pages
    }

    /// Returns the hard canonical-byte limit, or `None` when unlimited.
    #[must_use]
    pub const fn maximum_bytes(self) -> Option<NonZeroU64> {
        self.maximum_bytes
    }

    /// Reports whether a page-count and byte estimate fits both configured limits.
    #[must_use]
    pub fn permits(self, pages: u64, bytes: u64) -> bool {
        self.maximum_pages.is_none_or(|limit| pages <= limit.get())
            && self.maximum_bytes.is_none_or(|limit| bytes <= limit.get())
    }
}

/// A collection budget contained a zero-valued hard limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCollectionBudget {
    /// The maximum page count was zero.
    ZeroMaximumPages,
    /// The maximum byte count was zero.
    ZeroMaximumBytes,
}

impl fmt::Display for InvalidCollectionBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMaximumPages => {
                formatter.write_str("maximum pages must be greater than zero")
            }
            Self::ZeroMaximumBytes => {
                formatter.write_str("maximum bytes must be greater than zero")
            }
        }
    }
}

impl Error for InvalidCollectionBudget {}

/// The amount of public revision history retained for a selected page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryPolicy {
    /// Capture the current revision at selection time and every future revision.
    CurrentAndFuture,
    /// Capture the newest fixed number of revisions and every future revision.
    LastN(NonZeroU32),
    /// Capture revisions at or after this Unix timestamp and every future revision.
    Since(UnixTimestamp),
    /// Capture all public revisions made available by the source.
    Complete,
}

impl HistoryPolicy {
    /// Creates a `LastN` policy, rejecting zero because it would capture no head.
    pub fn last_n(count: u32) -> Result<Self, InvalidHistoryPolicy> {
        NonZeroU32::new(count)
            .map(Self::LastN)
            .ok_or(InvalidHistoryPolicy::ZeroRevisionCount)
    }
}

/// A UTC instant represented as seconds from the Unix epoch.
///
/// Signed values retain the full conventional Unix timestamp domain. Conversion to
/// MediaWiki's timestamp format belongs in the source adapter.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixTimestamp(i64);

impl UnixTimestamp {
    /// Creates a timestamp from seconds relative to 1970-01-01T00:00:00Z.
    #[must_use]
    pub const fn from_seconds(seconds: i64) -> Self {
        Self(seconds)
    }

    /// Returns seconds relative to 1970-01-01T00:00:00Z.
    #[must_use]
    pub const fn as_seconds(self) -> i64 {
        self.0
    }
}

/// A validation error for a revision-history policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidHistoryPolicy {
    /// A last-N policy requested zero revisions.
    ZeroRevisionCount,
}

impl fmt::Display for InvalidHistoryPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRevisionCount => {
                formatter.write_str("last-N history must include at least one revision")
            }
        }
    }
}

impl Error for InvalidHistoryPolicy {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ids_reject_zero() {
        assert_eq!(
            PageId::new(0),
            Err(InvalidId {
                kind: "MediaWiki page ID"
            })
        );
        assert_eq!(RevisionId::new(42).expect("positive ID").get(), 42);
    }

    #[test]
    fn page_titles_trim_and_reject_line_breaks() {
        assert_eq!(
            PageTitle::new("  Rust  ").expect("valid title").as_str(),
            "Rust"
        );
        assert_eq!(PageTitle::new(" \t "), Err(InvalidPageTitle::Empty));
        assert_eq!(
            PageTitle::new("unsafe\ntitle"),
            Err(InvalidPageTitle::ContainsControlCharacter)
        );
    }

    #[test]
    fn title_selection_is_non_empty_and_deduplicated() {
        assert_eq!(TitleSelection::new([]), Err(EmptyTitleSelection));

        let rust = PageTitle::new("Rust").expect("valid title");
        let selection = TitleSelection::new([rust.clone(), rust]).expect("non-empty");
        assert_eq!(selection.len(), 1);
        assert!(!selection.is_empty());
    }

    #[test]
    fn newline_title_lists_ignore_blanks_and_deduplicate() {
        let selection = TitleSelection::from_newline_delimited(" Rust \n\nFerris\nRust\n")
            .expect("valid title list");
        assert_eq!(
            selection.iter().map(PageTitle::as_str).collect::<Vec<_>>(),
            ["Ferris", "Rust"]
        );
        assert_eq!(
            TitleSelection::from_newline_delimited("\n \n"),
            Err(InvalidTitleList::Empty)
        );
    }

    #[test]
    fn collection_budgets_are_hard_and_zero_is_invalid() {
        let budget = CollectionBudget::unlimited()
            .with_maximum_pages(10)
            .expect("page budget")
            .with_maximum_bytes(1_024)
            .expect("byte budget");
        assert!(budget.permits(10, 1_024));
        assert!(!budget.permits(11, 1_024));
        assert!(!budget.permits(10, 1_025));
        assert_eq!(
            CollectionBudget::default().with_maximum_pages(0),
            Err(InvalidCollectionBudget::ZeroMaximumPages)
        );
    }

    #[test]
    fn last_n_history_requires_a_revision() {
        assert_eq!(
            HistoryPolicy::last_n(0),
            Err(InvalidHistoryPolicy::ZeroRevisionCount)
        );
        assert!(matches!(
            HistoryPolicy::last_n(10),
            Ok(HistoryPolicy::LastN(count)) if count.get() == 10
        ));
    }
}
