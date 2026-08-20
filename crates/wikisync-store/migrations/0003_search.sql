CREATE TABLE search_documents (
    search_id INTEGER PRIMARY KEY,
    wiki_id INTEGER NOT NULL,
    page_id INTEGER NOT NULL CHECK (page_id > 0),
    revision_id INTEGER NOT NULL CHECK (revision_id > 0),
    transformer_version TEXT NOT NULL,
    indexed_at INTEGER NOT NULL,
    UNIQUE (wiki_id, page_id),
    FOREIGN KEY (wiki_id, page_id) REFERENCES pages(wiki_id, page_id),
    FOREIGN KEY (wiki_id, revision_id) REFERENCES revisions(wiki_id, revision_id)
) STRICT;

CREATE VIRTUAL TABLE search_fts USING fts5(
    title,
    aliases,
    headings,
    body,
    categories,
    captions,
    content='',
    contentless_delete=1,
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3'
);
