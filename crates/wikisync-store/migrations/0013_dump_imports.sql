CREATE TABLE dump_imports (
    import_id INTEGER PRIMARY KEY CHECK (import_id > 0),
    run_id INTEGER NOT NULL UNIQUE
        REFERENCES sync_runs(run_id) ON DELETE CASCADE,
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    dump_digest TEXT NOT NULL
        CHECK (
            length(dump_digest) = 67
            AND substr(dump_digest, 1, 3) = 'b3:'
            AND substr(dump_digest, 4) NOT GLOB '*[^0-9a-f]*'
        ),
    dump_compressed_bytes INTEGER NOT NULL CHECK (dump_compressed_bytes > 0),
    collection_generation INTEGER NOT NULL CHECK (collection_generation > 0),
    configuration_hash TEXT NOT NULL
        CHECK (
            length(configuration_hash) = 67
            AND substr(configuration_hash, 1, 3) = 'b3:'
            AND substr(configuration_hash, 4) NOT GLOB '*[^0-9a-f]*'
        ),
    bootstrap_started_at INTEGER NOT NULL CHECK (bootstrap_started_at >= 0),
    state TEXT NOT NULL CHECK (state IN ('running', 'succeeded', 'failed')),
    pages_scanned INTEGER NOT NULL DEFAULT 0 CHECK (pages_scanned >= 0),
    imported_pages INTEGER NOT NULL DEFAULT 0 CHECK (imported_pages >= 0),
    imported_canonical_bytes INTEGER NOT NULL DEFAULT 0
        CHECK (imported_canonical_bytes >= 0),
    attempt_count INTEGER NOT NULL DEFAULT 1 CHECK (attempt_count > 0),
    retryable INTEGER NOT NULL DEFAULT 1 CHECK (retryable IN (0, 1)),
    error_code TEXT,
    error_message TEXT,
    created_at INTEGER NOT NULL,
    claimed_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    finished_at INTEGER,
    CHECK (
        (state = 'running' AND finished_at IS NULL
            AND error_code IS NULL AND error_message IS NULL)
        OR (state = 'succeeded' AND finished_at IS NOT NULL
            AND retryable = 0 AND error_code IS NULL AND error_message IS NULL)
        OR (state = 'failed' AND finished_at IS NOT NULL
            AND error_code IS NOT NULL AND error_message IS NOT NULL)
    )
) STRICT;

CREATE INDEX dump_imports_by_scope_state
    ON dump_imports(wiki_id, collection_id, state, import_id);

CREATE TABLE dump_import_pages (
    import_id INTEGER NOT NULL
        REFERENCES dump_imports(import_id) ON DELETE CASCADE,
    wiki_id INTEGER NOT NULL,
    page_id INTEGER NOT NULL CHECK (page_id > 0),
    revision_id INTEGER NOT NULL CHECK (revision_id > 0),
    canonical_bytes INTEGER NOT NULL CHECK (canonical_bytes >= 0),
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (import_id, page_id),
    FOREIGN KEY (wiki_id, page_id) REFERENCES pages(wiki_id, page_id),
    FOREIGN KEY (wiki_id, revision_id) REFERENCES revisions(wiki_id, revision_id)
) STRICT;
