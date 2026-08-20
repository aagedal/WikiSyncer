//! Search abstractions and the SQLite FTS5 implementation.
//!
//! Only current-page metadata is stored alongside a contentless index. Searchable
//! article text remains a rebuildable representation of canonical objects and is not
//! duplicated as an ordinary SQLite column.

use std::error::Error;
use std::fmt;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use wikisync_core::{PageId, PageTitle, RevisionId, WikiId};
use wikisync_store::Library;

/// Maximum number of hits returned by one query.
pub const MAX_SEARCH_RESULTS: u32 = 100;

/// Separately weighted fields for one selected current revision.
#[derive(Clone, Copy, Debug)]
pub struct SearchDocument<'a> {
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// Stable page identity.
    pub page_id: PageId,
    /// Current captured revision identity.
    pub revision_id: RevisionId,
    /// Current canonical title.
    pub title: &'a PageTitle,
    /// Previously observed titles, separated by line feeds.
    pub aliases: &'a str,
    /// Normalized headings, separated by line feeds.
    pub headings: &'a str,
    /// Full normalized reading text.
    pub body: &'a str,
    /// Selected category names, separated by line feeds.
    pub categories: &'a str,
    /// Captured media captions, separated by line feeds.
    pub captions: &'a str,
    /// Deterministic content-transformer identifier.
    pub transformer_version: &'a str,
}

/// Parameters for one FTS query.
#[derive(Clone, Copy, Debug)]
pub struct SearchQuery<'a> {
    /// FTS5 query expression.
    pub text: &'a str,
    /// Optional source-wiki filter.
    pub wiki_id: Option<WikiId>,
    /// Maximum number of returned hits.
    pub limit: u32,
}

impl<'a> SearchQuery<'a> {
    /// Creates a query using a 20-result default limit.
    #[must_use]
    pub const fn new(text: &'a str) -> Self {
        Self {
            text,
            wiki_id: None,
            limit: 20,
        }
    }

    /// Restricts results to one source wiki.
    #[must_use]
    pub const fn for_wiki(mut self, wiki_id: WikiId) -> Self {
        self.wiki_id = Some(wiki_id);
        self
    }

    /// Sets the result limit. Validation occurs when the query runs.
    #[must_use]
    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }
}

/// One ranked selected-current-page match.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    /// Source wiki identity.
    pub wiki_id: WikiId,
    /// Stable page identity.
    pub page_id: PageId,
    /// Indexed current revision identity.
    pub revision_id: RevisionId,
    /// Current canonical title.
    pub title: PageTitle,
    /// FTS5 BM25 score; lower values are more relevant.
    pub rank: f64,
}

/// Common behavior exposed by search backends.
pub trait SearchIndex {
    /// Replaces the indexed representation for one current page.
    fn index_document(&mut self, document: &SearchDocument<'_>) -> Result<(), SearchError>;

    /// Returns ranked current-page matches.
    fn search(&self, query: SearchQuery<'_>) -> Result<Vec<SearchHit>, SearchError>;
}

/// Contentless FTS5 index stored in the library database.
#[derive(Debug)]
pub struct SqliteSearchIndex {
    connection: Connection,
}

impl SqliteSearchIndex {
    /// Opens the already-migrated database for a library.
    pub fn open(library: &Library) -> Result<Self, SearchError> {
        let connection = Connection::open(library.database_path())?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 3 {
            return Err(SearchError::MissingSchema(version));
        }
        Ok(Self { connection })
    }
}

impl SearchIndex for SqliteSearchIndex {
    fn index_document(&mut self, document: &SearchDocument<'_>) -> Result<(), SearchError> {
        if document.transformer_version.trim().is_empty() {
            return Err(SearchError::InvalidDocument(
                "transformer version must be non-empty",
            ));
        }
        let wiki_id = sql_integer(document.wiki_id.get())?;
        let page_id = sql_integer(document.page_id.get())?;
        let revision_id = sql_integer(document.revision_id.get())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current_revision = transaction
            .query_row(
                "SELECT current_revision_id FROM pages
                 WHERE wiki_id = ?1 AND page_id = ?2",
                params![wiki_id, page_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        if current_revision != Some(revision_id) {
            return Err(SearchError::InvalidDocument(
                "only a page's selected current revision can be indexed",
            ));
        }

        let old_search_id = transaction
            .query_row(
                "SELECT search_id FROM search_documents
                 WHERE wiki_id = ?1 AND page_id = ?2",
                params![wiki_id, page_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(search_id) = old_search_id {
            transaction.execute("DELETE FROM search_fts WHERE rowid = ?1", [search_id])?;
        }
        transaction.execute(
            "INSERT INTO search_documents (
                wiki_id, page_id, revision_id, transformer_version, indexed_at
             ) VALUES (?1, ?2, ?3, ?4, unixepoch())
             ON CONFLICT(wiki_id, page_id) DO UPDATE SET
                revision_id = excluded.revision_id,
                transformer_version = excluded.transformer_version,
                indexed_at = excluded.indexed_at",
            params![wiki_id, page_id, revision_id, document.transformer_version],
        )?;
        let search_id: i64 = transaction.query_row(
            "SELECT search_id FROM search_documents
             WHERE wiki_id = ?1 AND page_id = ?2",
            params![wiki_id, page_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO search_fts (
                rowid, title, aliases, headings, body, categories, captions
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                search_id,
                document.title.as_str(),
                document.aliases,
                document.headings,
                document.body,
                document.categories,
                document.captions,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn search(&self, query: SearchQuery<'_>) -> Result<Vec<SearchHit>, SearchError> {
        if query.text.trim().is_empty() {
            return Err(SearchError::EmptyQuery);
        }
        if !(1..=MAX_SEARCH_RESULTS).contains(&query.limit) {
            return Err(SearchError::InvalidLimit(query.limit));
        }
        let wiki_filter = query
            .wiki_id
            .map(|wiki_id| sql_integer(wiki_id.get()))
            .transpose()?;
        let mut statement = self.connection.prepare(
            "SELECT documents.wiki_id, documents.page_id, documents.revision_id,
                    pages.current_title,
                    bm25(search_fts, 10.0, 6.0, 4.0, 1.0, 2.0, 2.0)
             FROM search_fts
             JOIN search_documents AS documents ON documents.search_id = search_fts.rowid
             JOIN pages USING (wiki_id, page_id)
             WHERE search_fts MATCH ?1
               AND (?2 IS NULL OR documents.wiki_id = ?2)
               AND pages.current_revision_id = documents.revision_id
             ORDER BY 5, pages.current_title, documents.wiki_id, documents.page_id
             LIMIT ?3",
        )?;
        let rows = statement
            .query_map(
                params![query.text, wiki_filter, i64::from(query.limit)],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(wiki_id, page_id, revision_id, title, rank)| {
                Ok(SearchHit {
                    wiki_id: wiki_id_from_sql(wiki_id)?,
                    page_id: page_id_from_sql(page_id)?,
                    revision_id: revision_id_from_sql(revision_id)?,
                    title: PageTitle::new(title)
                        .map_err(|_| SearchError::CorruptMetadata("invalid page title"))?,
                    rank,
                })
            })
            .collect()
    }
}

fn sql_integer(value: u64) -> Result<i64, SearchError> {
    i64::try_from(value).map_err(|_| SearchError::IntegerOutOfRange(value))
}

fn positive_sql(value: i64, message: &'static str) -> Result<u64, SearchError> {
    let value = u64::try_from(value).map_err(|_| SearchError::CorruptMetadata(message))?;
    (value > 0)
        .then_some(value)
        .ok_or(SearchError::CorruptMetadata(message))
}

fn wiki_id_from_sql(value: i64) -> Result<WikiId, SearchError> {
    WikiId::new(positive_sql(value, "invalid wiki ID")?)
        .map_err(|_| SearchError::CorruptMetadata("invalid wiki ID"))
}

fn page_id_from_sql(value: i64) -> Result<PageId, SearchError> {
    PageId::new(positive_sql(value, "invalid page ID")?)
        .map_err(|_| SearchError::CorruptMetadata("invalid page ID"))
}

fn revision_id_from_sql(value: i64) -> Result<RevisionId, SearchError> {
    RevisionId::new(positive_sql(value, "invalid revision ID")?)
        .map_err(|_| SearchError::CorruptMetadata("invalid revision ID"))
}

/// Search configuration, schema, or SQLite failure.
#[derive(Debug)]
pub enum SearchError {
    /// SQLite operation failed, including invalid FTS syntax.
    Sqlite(rusqlite::Error),
    /// The search query was empty.
    EmptyQuery,
    /// The requested result count was outside the supported range.
    InvalidLimit(u32),
    /// A document violated an indexing invariant.
    InvalidDocument(&'static str),
    /// Search was opened against a library without the required migration.
    MissingSchema(u32),
    /// Persisted search metadata violated an invariant.
    CorruptMetadata(&'static str),
    /// An identifier exceeds SQLite's signed integer range.
    IntegerOutOfRange(u64),
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite search error: {error}"),
            Self::EmptyQuery => formatter.write_str("search query must be non-empty"),
            Self::InvalidLimit(limit) => write!(
                formatter,
                "search result limit {limit} is outside 1..={MAX_SEARCH_RESULTS}"
            ),
            Self::InvalidDocument(message) => formatter.write_str(message),
            Self::MissingSchema(version) => write!(
                formatter,
                "library schema version {version} does not contain the search index"
            ),
            Self::CorruptMetadata(message) => {
                write!(formatter, "corrupt search metadata: {message}")
            }
            Self::IntegerOutOfRange(value) => {
                write!(formatter, "value {value} exceeds SQLite integer range")
            }
        }
    }
}

impl Error for SearchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for SearchError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikisync_content::to_search_content;
    use wikisync_store::CurrentRevisionCapture;

    fn fixture() -> (
        tempfile::TempDir,
        Library,
        WikiId,
        PageId,
        RevisionId,
        PageTitle,
    ) {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Languages")
            .expect("collection");
        let page_id = PageId::new(25_357_340).expect("page ID");
        let revision_id = RevisionId::new(1_300_000_001).expect("revision ID");
        let title = PageTitle::new("Rust (programming language)").expect("title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id,
                    namespace: 0,
                    title: &title,
                    revision_id,
                    parent_id: None,
                    timestamp: "2026-08-20T00:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"== Memory safety ==\nRust is a systems programming language.",
                },
            )
            .expect("capture");
        (directory, library, wiki_id, page_id, revision_id, title)
    }

    #[test]
    fn indexes_and_finds_weighted_current_content() {
        let (_directory, library, wiki_id, page_id, revision_id, title) = fixture();
        let content =
            to_search_content("== Memory safety ==\nRust is a systems programming language.");
        let mut index = SqliteSearchIndex::open(&library).expect("index");
        index
            .index_document(&SearchDocument {
                wiki_id,
                page_id,
                revision_id,
                title: &title,
                aliases: "Rust language",
                headings: &content.headings,
                body: &content.body,
                categories: "Systems programming languages",
                captions: "",
                transformer_version: content.transformer_version.as_str(),
            })
            .expect("index document");

        let hits = index
            .search(SearchQuery::new("memory AND safety"))
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].page_id, page_id);
        assert_eq!(hits[0].revision_id, revision_id);
        assert_eq!(hits[0].title, title);
    }

    #[test]
    fn replacing_a_document_removes_old_tokens() {
        let (_directory, library, wiki_id, page_id, revision_id, title) = fixture();
        let mut index = SqliteSearchIndex::open(&library).expect("index");
        let base = SearchDocument {
            wiki_id,
            page_id,
            revision_id,
            title: &title,
            aliases: "",
            headings: "",
            body: "obsolete vocabulary",
            categories: "",
            captions: "",
            transformer_version: "test-v1",
        };
        index.index_document(&base).expect("first index");
        index
            .index_document(&SearchDocument {
                body: "replacement terminology",
                ..base
            })
            .expect("replacement index");

        assert!(
            index
                .search(SearchQuery::new("obsolete"))
                .expect("old query")
                .is_empty()
        );
        assert_eq!(
            index
                .search(SearchQuery::new("replacement"))
                .expect("new query")
                .len(),
            1
        );
    }

    #[test]
    fn validates_query_bounds() {
        let (_directory, library, ..) = fixture();
        let index = SqliteSearchIndex::open(&library).expect("index");
        assert!(matches!(
            index.search(SearchQuery::new("  ")),
            Err(SearchError::EmptyQuery)
        ));
        assert!(matches!(
            index.search(SearchQuery::new("rust").with_limit(0)),
            Err(SearchError::InvalidLimit(0))
        ));
    }
}
