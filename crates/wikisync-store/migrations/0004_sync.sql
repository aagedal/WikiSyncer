CREATE TABLE sync_runs (
    run_id INTEGER PRIMARY KEY CHECK (run_id > 0),
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    collection_id INTEGER REFERENCES collections(collection_id) ON DELETE SET NULL,
    run_kind TEXT NOT NULL
        CHECK (run_kind IN ('bootstrap', 'update', 'history', 'reconciliation')),
    state TEXT NOT NULL DEFAULT 'running'
        CHECK (state IN ('running', 'succeeded', 'cancelled')),
    window_start INTEGER NOT NULL CHECK (window_start >= 0),
    checkpoint_candidate INTEGER NOT NULL CHECK (checkpoint_candidate >= window_start),
    created_at INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    CHECK (
        (state = 'running' AND finished_at IS NULL)
        OR (state IN ('succeeded', 'cancelled') AND finished_at IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX one_running_sync_per_scope
    ON sync_runs(wiki_id, IFNULL(collection_id, 0))
    WHERE state = 'running';

CREATE TABLE sync_jobs (
    job_id INTEGER PRIMARY KEY CHECK (job_id > 0),
    run_id INTEGER NOT NULL REFERENCES sync_runs(run_id) ON DELETE CASCADE,
    job_key TEXT NOT NULL CHECK (length(job_key) > 0),
    job_kind TEXT NOT NULL CHECK (length(job_kind) > 0),
    subject TEXT,
    state TEXT NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'running', 'succeeded', 'failed')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    retryable INTEGER NOT NULL DEFAULT 1 CHECK (retryable IN (0, 1)),
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    UNIQUE (run_id, job_key),
    CHECK (
        (state = 'queued' AND finished_at IS NULL)
        OR (state = 'running' AND started_at IS NOT NULL AND finished_at IS NULL)
        OR (state IN ('succeeded', 'failed') AND finished_at IS NOT NULL)
    )
) STRICT;

CREATE INDEX sync_jobs_next
    ON sync_jobs(run_id, state, job_id);

CREATE TABLE sync_errors (
    error_id INTEGER PRIMARY KEY CHECK (error_id > 0),
    run_id INTEGER NOT NULL REFERENCES sync_runs(run_id) ON DELETE CASCADE,
    job_id INTEGER REFERENCES sync_jobs(job_id) ON DELETE CASCADE,
    code TEXT NOT NULL CHECK (length(code) > 0),
    message TEXT NOT NULL CHECK (length(message) > 0),
    retryable INTEGER NOT NULL CHECK (retryable IN (0, 1)),
    occurred_at INTEGER NOT NULL
) STRICT;

CREATE INDEX sync_errors_by_run
    ON sync_errors(run_id, occurred_at DESC, error_id DESC);

CREATE TABLE sync_checkpoints (
    checkpoint_id INTEGER PRIMARY KEY CHECK (checkpoint_id > 0),
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    collection_id INTEGER REFERENCES collections(collection_id) ON DELETE CASCADE,
    committed_through INTEGER NOT NULL CHECK (committed_through >= 0),
    overlap_seconds INTEGER NOT NULL CHECK (overlap_seconds > 0),
    recent_changes_cursor TEXT,
    reconciled_at INTEGER CHECK (reconciled_at >= 0),
    last_run_id INTEGER REFERENCES sync_runs(run_id),
    updated_at INTEGER NOT NULL
) STRICT;

CREATE UNIQUE INDEX sync_checkpoint_scope
    ON sync_checkpoints(wiki_id, IFNULL(collection_id, 0));
