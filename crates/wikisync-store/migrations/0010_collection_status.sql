ALTER TABLE collections ADD COLUMN tombstoned_at INTEGER
    CHECK (tombstoned_at IS NULL OR tombstoned_at >= 0);

ALTER TABLE collections ADD COLUMN generation INTEGER NOT NULL DEFAULT 1
    CHECK (generation > 0);

ALTER TABLE collections ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
    CHECK (
        (status = 'active' AND tombstoned_at IS NULL)
        OR (status = 'tombstoned' AND tombstoned_at IS NOT NULL)
    );

CREATE INDEX collections_by_status_name
    ON collections(status, name, collection_id);
