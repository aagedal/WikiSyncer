CREATE TABLE collection_schedules (
    collection_id INTEGER PRIMARY KEY
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    cadence_kind TEXT NOT NULL
        CHECK (cadence_kind IN ('manual', 'interval', 'daily-utc')),
    cadence_seconds INTEGER,
    jitter_seconds INTEGER NOT NULL DEFAULT 0
        CHECK (jitter_seconds BETWEEN 0 AND 86400),
    paused INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    next_run_at INTEGER CHECK (next_run_at >= 0),
    last_started_at INTEGER CHECK (last_started_at >= 0),
    updated_at INTEGER NOT NULL,
    CHECK (
        (cadence_kind = 'manual' AND cadence_seconds IS NULL
            AND jitter_seconds = 0 AND next_run_at IS NULL)
        OR (cadence_kind = 'interval'
            AND cadence_seconds IS NOT NULL
            AND cadence_seconds BETWEEN 60 AND 31622400
            AND jitter_seconds <= cadence_seconds
            AND next_run_at IS NOT NULL)
        OR (cadence_kind = 'daily-utc'
            AND cadence_seconds IS NOT NULL
            AND cadence_seconds BETWEEN 0 AND 86399
            AND next_run_at IS NOT NULL)
    )
) STRICT;

CREATE INDEX collection_schedules_due
    ON collection_schedules(next_run_at, collection_id)
    WHERE paused = 0 AND cadence_kind != 'manual';

INSERT INTO collection_schedules (
    collection_id, cadence_kind, cadence_seconds, jitter_seconds,
    paused, next_run_at, last_started_at, updated_at
)
SELECT collection_id, 'manual', NULL, 0, 0, NULL, NULL, created_at
FROM collections;
