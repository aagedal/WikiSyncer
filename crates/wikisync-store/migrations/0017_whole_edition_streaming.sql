CREATE TABLE whole_edition_imports (
    import_id INTEGER PRIMARY KEY
        REFERENCES dump_imports(import_id) ON DELETE CASCADE,
    source_endpoint TEXT NOT NULL
        CHECK (length(CAST(source_endpoint AS BLOB)) BETWEEN 1 AND 16384),
    snapshot_id TEXT NOT NULL
        CHECK (length(CAST(snapshot_id AS BLOB)) BETWEEN 1 AND 16384),
    snapshot_timestamp INTEGER NOT NULL CHECK (snapshot_timestamp >= 0),
    race_window_end INTEGER NOT NULL CHECK (race_window_end >= snapshot_timestamp),
    artifact_index INTEGER NOT NULL DEFAULT 0 CHECK (artifact_index >= 0),
    artifact_offset INTEGER NOT NULL DEFAULT 0 CHECK (artifact_offset >= 0),
    recovery_marker_id INTEGER,
    FOREIGN KEY (recovery_marker_id)
        REFERENCES whole_edition_recovery_markers(recovery_marker_id)
) STRICT;

CREATE INDEX whole_edition_imports_by_snapshot
    ON whole_edition_imports(source_endpoint, snapshot_id, import_id);

CREATE TABLE whole_edition_recovery_markers (
    recovery_marker_id INTEGER PRIMARY KEY CHECK (recovery_marker_id > 0),
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    last_safe_checkpoint INTEGER NOT NULL CHECK (last_safe_checkpoint >= 0),
    detected_at INTEGER NOT NULL CHECK (detected_at >= last_safe_checkpoint),
    reason TEXT NOT NULL CHECK (length(CAST(reason AS BLOB)) BETWEEN 1 AND 16384),
    state TEXT NOT NULL CHECK (state IN ('required', 'recovering', 'resolved')),
    import_id INTEGER UNIQUE REFERENCES dump_imports(import_id),
    discovery_id INTEGER UNIQUE,
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    CHECK (
        (state = 'required' AND import_id IS NULL AND discovery_id IS NULL
            AND resolved_at IS NULL)
        OR (state = 'recovering' AND import_id IS NOT NULL AND resolved_at IS NULL)
        OR (state = 'resolved' AND import_id IS NOT NULL AND discovery_id IS NOT NULL
            AND resolved_at IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX one_unresolved_whole_edition_recovery
    ON whole_edition_recovery_markers(collection_id)
    WHERE state != 'resolved';

CREATE TABLE whole_edition_discoveries (
    discovery_id INTEGER PRIMARY KEY CHECK (discovery_id > 0),
    run_id INTEGER NOT NULL UNIQUE
        REFERENCES sync_runs(run_id) ON DELETE CASCADE,
    import_id INTEGER REFERENCES whole_edition_imports(import_id),
    recovery_marker_id INTEGER
        REFERENCES whole_edition_recovery_markers(recovery_marker_id),
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    discovery_kind TEXT NOT NULL
        CHECK (discovery_kind IN ('race-window', 'incremental', 'long-gap-closure')),
    window_start INTEGER NOT NULL CHECK (window_start >= 0),
    window_end INTEGER NOT NULL CHECK (window_end >= window_start),
    state TEXT NOT NULL CHECK (state IN ('running', 'succeeded', 'failed')),
    continuation TEXT
        CHECK (continuation IS NULL OR length(CAST(continuation AS BLOB)) BETWEEN 1 AND 16384),
    source_exhausted INTEGER NOT NULL DEFAULT 0 CHECK (source_exhausted IN (0, 1)),
    batches_recorded INTEGER NOT NULL DEFAULT 0 CHECK (batches_recorded >= 0),
    changes_observed INTEGER NOT NULL DEFAULT 0 CHECK (changes_observed >= 0),
    new_changes INTEGER NOT NULL DEFAULT 0 CHECK (new_changes >= 0),
    applied_changes INTEGER NOT NULL DEFAULT 0 CHECK (applied_changes >= 0),
    ignored_changes INTEGER NOT NULL DEFAULT 0 CHECK (ignored_changes >= 0),
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
            AND retryable = 0 AND source_exhausted = 1
            AND error_code IS NULL AND error_message IS NULL)
        OR (state = 'failed' AND finished_at IS NOT NULL
            AND error_code IS NOT NULL AND error_message IS NOT NULL)
    ),
    CHECK (
        (discovery_kind = 'incremental' AND import_id IS NULL
            AND recovery_marker_id IS NULL)
        OR (discovery_kind = 'race-window' AND import_id IS NOT NULL
            AND recovery_marker_id IS NULL)
        OR (discovery_kind = 'long-gap-closure' AND import_id IS NOT NULL
            AND recovery_marker_id IS NOT NULL)
    )
) STRICT;

CREATE INDEX whole_edition_discoveries_by_scope_state
    ON whole_edition_discoveries(collection_id, state, discovery_id);

CREATE TABLE whole_edition_changes (
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    change_id INTEGER NOT NULL CHECK (change_id > 0),
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    change_kind TEXT NOT NULL
        CHECK (change_kind IN ('edit', 'new', 'move', 'delete', 'restore', 'other')),
    occurred_at INTEGER NOT NULL CHECK (occurred_at >= 0),
    page_id INTEGER CHECK (page_id > 0),
    revision_id INTEGER CHECK (revision_id > 0),
    namespace INTEGER,
    title TEXT CHECK (title IS NULL OR length(CAST(title AS BLOB)) BETWEEN 1 AND 16384),
    application_state TEXT NOT NULL
        CHECK (application_state IN ('pending', 'applied', 'ignored')),
    first_discovery_id INTEGER NOT NULL
        REFERENCES whole_edition_discoveries(discovery_id),
    last_discovery_id INTEGER NOT NULL
        REFERENCES whole_edition_discoveries(discovery_id),
    first_observed_at INTEGER NOT NULL,
    last_observed_at INTEGER NOT NULL,
    applied_at INTEGER,
    PRIMARY KEY (collection_id, change_id),
    CHECK (
        (application_state = 'pending' AND applied_at IS NULL)
        OR (application_state IN ('applied', 'ignored') AND applied_at IS NOT NULL)
    )
) STRICT;

CREATE INDEX whole_edition_changes_pending
    ON whole_edition_changes(collection_id, application_state, change_id);

CREATE TABLE whole_edition_discovery_changes (
    discovery_id INTEGER NOT NULL
        REFERENCES whole_edition_discoveries(discovery_id) ON DELETE CASCADE,
    collection_id INTEGER NOT NULL,
    change_id INTEGER NOT NULL,
    PRIMARY KEY (discovery_id, change_id),
    FOREIGN KEY (collection_id, change_id)
        REFERENCES whole_edition_changes(collection_id, change_id)
) STRICT;
