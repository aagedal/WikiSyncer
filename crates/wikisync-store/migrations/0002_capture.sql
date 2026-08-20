CREATE TABLE wikis (
    wiki_id INTEGER PRIMARY KEY CHECK (wiki_id > 0),
    api_endpoint TEXT NOT NULL UNIQUE,
    language_code TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE collections (
    collection_id INTEGER PRIMARY KEY CHECK (collection_id > 0),
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    name TEXT NOT NULL,
    rule_kind TEXT NOT NULL CHECK (rule_kind = 'explicit-titles'),
    history_policy TEXT NOT NULL CHECK (history_policy = 'current-and-future'),
    created_at INTEGER NOT NULL,
    UNIQUE (wiki_id, name)
) STRICT;

CREATE TABLE pages (
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    page_id INTEGER NOT NULL CHECK (page_id > 0),
    namespace INTEGER NOT NULL,
    current_title TEXT NOT NULL,
    current_revision_id INTEGER CHECK (current_revision_id > 0),
    current_revision_time TEXT,
    state TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'missing', 'deleted')),
    first_captured_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (wiki_id, page_id)
) STRICT;

CREATE TABLE page_titles (
    wiki_id INTEGER NOT NULL,
    page_id INTEGER NOT NULL,
    title TEXT NOT NULL,
    first_observed_at INTEGER NOT NULL,
    last_observed_at INTEGER NOT NULL,
    PRIMARY KEY (wiki_id, page_id, title),
    FOREIGN KEY (wiki_id, page_id) REFERENCES pages(wiki_id, page_id)
) STRICT;

CREATE TABLE revisions (
    wiki_id INTEGER NOT NULL,
    revision_id INTEGER NOT NULL CHECK (revision_id > 0),
    page_id INTEGER NOT NULL CHECK (page_id > 0),
    parent_revision_id INTEGER CHECK (parent_revision_id > 0),
    revision_time TEXT NOT NULL,
    author_name TEXT,
    author_id INTEGER CHECK (author_id > 0),
    comment TEXT,
    is_minor INTEGER NOT NULL CHECK (is_minor IN (0, 1)),
    source_size INTEGER NOT NULL CHECK (source_size >= 0),
    upstream_sha1 TEXT,
    content_model TEXT NOT NULL,
    content_object_id TEXT NOT NULL REFERENCES content_objects(object_id),
    captured_at INTEGER NOT NULL,
    PRIMARY KEY (wiki_id, revision_id),
    FOREIGN KEY (wiki_id, page_id) REFERENCES pages(wiki_id, page_id)
) STRICT;

CREATE INDEX revisions_by_page_time
    ON revisions(wiki_id, page_id, revision_time DESC, revision_id DESC);

CREATE TABLE collection_pages (
    collection_id INTEGER NOT NULL REFERENCES collections(collection_id) ON DELETE CASCADE,
    wiki_id INTEGER NOT NULL,
    page_id INTEGER NOT NULL,
    inclusion_reason TEXT NOT NULL CHECK (inclusion_reason = 'explicit-title'),
    added_at INTEGER NOT NULL,
    PRIMARY KEY (collection_id, wiki_id, page_id),
    FOREIGN KEY (wiki_id, page_id) REFERENCES pages(wiki_id, page_id)
) STRICT;

CREATE TABLE unresolved_titles (
    collection_id INTEGER NOT NULL REFERENCES collections(collection_id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    namespace INTEGER NOT NULL,
    last_observed_at INTEGER NOT NULL,
    PRIMARY KEY (collection_id, title)
) STRICT;
