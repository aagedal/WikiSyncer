//! Domain identities and policies shared by WikiSyncer interfaces and services.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

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

/// The rule used to resolve membership in a collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectionRule {
    /// A fixed set of titles, whether entered directly or imported from a title list.
    ExplicitTitles(TitleSelection),
    /// Pages reachable from a category, including the category at depth zero.
    Category {
        /// The source category title.
        title: PageTitle,
        /// Maximum number of subcategory edges to traverse.
        recursion_depth: u16,
    },
}

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
