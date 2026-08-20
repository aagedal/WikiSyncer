use similar::{ChangeTag, TextDiff};

use crate::to_plain_text;

/// Canonical or reading-oriented input used for a local revision comparison.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiffMode {
    /// Compare the exact canonical wikitext bytes after UTF-8 decoding.
    ExactSource,
    /// Compare deterministic normalized plain text derived from each revision.
    Reading,
}

impl DiffMode {
    /// Returns the stable command/JSON name for this mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactSource => "source",
            Self::Reading => "reading",
        }
    }
}

/// The role of one line or inline span in a comparison.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiffTag {
    /// Text present in both inputs.
    Equal,
    /// Text present only in the older input.
    Delete,
    /// Text present only in the newer input.
    Insert,
}

impl DiffTag {
    /// Returns the stable JSON name for this tag.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Equal => "equal",
            Self::Delete => "delete",
            Self::Insert => "insert",
        }
    }
}

impl From<ChangeTag> for DiffTag {
    fn from(tag: ChangeTag) -> Self {
        match tag {
            ChangeTag::Equal => Self::Equal,
            ChangeTag::Delete => Self::Delete,
            ChangeTag::Insert => Self::Insert,
        }
    }
}

/// One word-level segment within a line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffSpan {
    /// Whether this segment is common, removed, or added.
    pub tag: DiffTag,
    /// Exact segment text in the selected representation.
    pub text: String,
}

/// One line-level comparison record with optional word-level refinement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    /// Whether the line is common, removed, or added.
    pub tag: DiffTag,
    /// One-based line number in the older input.
    pub old_line: Option<usize>,
    /// One-based line number in the newer input.
    pub new_line: Option<usize>,
    /// Ordered word-level segments that reconstruct this line exactly.
    pub spans: Vec<DiffSpan>,
}

/// A complete deterministic local comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentDiff {
    /// Representation compared by this result.
    pub mode: DiffMode,
    /// Line records in display order.
    pub lines: Vec<DiffLine>,
}

impl ContentDiff {
    /// Returns whether the two selected representations differ.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        self.lines.iter().any(|line| line.tag != DiffTag::Equal)
    }
}

/// Compares two canonical revisions entirely offline.
///
/// Line alignment is refined by a word-level comparison for paired replacement
/// lines. The resulting spans reconstruct every emitted line without rewriting the
/// canonical source or persisting another representation.
#[must_use]
pub fn diff(older_source: &str, newer_source: &str, mode: DiffMode) -> ContentDiff {
    let (older, newer) = match mode {
        DiffMode::ExactSource => (older_source.to_owned(), newer_source.to_owned()),
        DiffMode::Reading => (to_plain_text(older_source), to_plain_text(newer_source)),
    };
    let line_diff = TextDiff::from_lines(&older, &newer);
    let mut lines = line_diff
        .iter_all_changes()
        .map(|change| {
            let tag = DiffTag::from(change.tag());
            DiffLine {
                tag,
                old_line: change.old_index().map(|index| index + 1),
                new_line: change.new_index().map(|index| index + 1),
                spans: vec![DiffSpan {
                    tag,
                    text: change.value().to_owned(),
                }],
            }
        })
        .collect::<Vec<_>>();
    refine_replacements(&mut lines);
    ContentDiff { mode, lines }
}

fn refine_replacements(lines: &mut [DiffLine]) {
    let mut start = 0;
    while start < lines.len() {
        if lines[start].tag == DiffTag::Equal {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < lines.len() && lines[end].tag != DiffTag::Equal {
            end += 1;
        }
        let deleted = (start..end)
            .filter(|index| lines[*index].tag == DiffTag::Delete)
            .collect::<Vec<_>>();
        let inserted = (start..end)
            .filter(|index| lines[*index].tag == DiffTag::Insert)
            .collect::<Vec<_>>();
        for (deleted_index, inserted_index) in deleted.into_iter().zip(inserted) {
            let old = line_text(&lines[deleted_index]);
            let new = line_text(&lines[inserted_index]);
            let word_diff = TextDiff::from_words(&old, &new);
            lines[deleted_index].spans = word_diff
                .iter_all_changes()
                .filter(|change| change.tag() != ChangeTag::Insert)
                .map(|change| DiffSpan {
                    tag: DiffTag::from(change.tag()),
                    text: change.value().to_owned(),
                })
                .collect();
            lines[inserted_index].spans = word_diff
                .iter_all_changes()
                .filter(|change| change.tag() != ChangeTag::Delete)
                .map(|change| DiffSpan {
                    tag: DiffTag::from(change.tag()),
                    text: change.value().to_owned(),
                })
                .collect();
        }
        start = end;
    }
}

fn line_text(line: &DiffLine) -> String {
    line.spans.iter().map(|span| span.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_diff_has_line_numbers_and_word_refinement() {
        let result = diff(
            "Heading\nA systems language.\n",
            "Heading\nA memory-safe language.\n",
            DiffMode::ExactSource,
        );

        assert!(result.has_changes());
        assert_eq!(result.lines[0].tag, DiffTag::Equal);
        assert_eq!(result.lines[0].old_line, Some(1));
        let deletion = result
            .lines
            .iter()
            .find(|line| line.tag == DiffTag::Delete)
            .expect("deleted line");
        assert!(
            deletion
                .spans
                .iter()
                .any(|span| { span.tag == DiffTag::Delete && span.text.contains("systems") })
        );
        let insertion = result
            .lines
            .iter()
            .find(|line| line.tag == DiffTag::Insert)
            .expect("inserted line");
        assert!(
            insertion
                .spans
                .iter()
                .any(|span| { span.tag == DiffTag::Insert && span.text.contains("memory-safe") })
        );
    }

    #[test]
    fn reading_diff_ignores_wikitext_formatting_only_changes() {
        let result = diff(
            "A [[Rust]] article.\n",
            "A Rust article.\n",
            DiffMode::Reading,
        );
        assert!(!result.has_changes());
        assert_eq!(result.mode.as_str(), "reading");
    }
}
