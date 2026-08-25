ALTER TABLE purge_operations ADD COLUMN catalog_fingerprint TEXT CHECK (
    catalog_fingerprint IS NULL OR (
        length(catalog_fingerprint) = 67
        AND substr(catalog_fingerprint, 1, 3) = 'b3:'
        AND substr(catalog_fingerprint, 4) NOT GLOB '*[^0-9a-f]*'
    )
);

-- Schema-14 journals have no independent logical-catalog commitment and must never
-- become cleanup-capable by migration. Preserve their audit rows, release the
-- unfinished-collection uniqueness slot, and require a fresh preview/authorization.
UPDATE purge_operations
SET state = 'failed', finished_at = updated_at
WHERE catalog_fingerprint IS NULL
  AND state IN ('authorized', 'repacking', 'cleaning');

CREATE TABLE purge_authorized_absences (
    purge_id INTEGER NOT NULL,
    object_id TEXT NOT NULL,
    absent_at INTEGER NOT NULL CHECK (absent_at >= 0),
    superseded_at INTEGER CHECK (
        superseded_at IS NULL OR superseded_at >= absent_at
    ),
    PRIMARY KEY (purge_id, object_id),
    FOREIGN KEY (purge_id, object_id)
        REFERENCES purge_objects(purge_id, object_id)
) STRICT;

CREATE UNIQUE INDEX one_active_purge_absence_per_object
    ON purge_authorized_absences(object_id)
    WHERE superseded_at IS NULL;

CREATE INDEX purge_authorized_absences_by_object
    ON purge_authorized_absences(object_id, purge_id);

CREATE TABLE purge_file_work (
    purge_id INTEGER NOT NULL
        REFERENCES purge_operations(purge_id) ON DELETE CASCADE,
    file_kind TEXT NOT NULL CHECK (file_kind IN ('loose', 'pack', 'index')),
    relative_path TEXT NOT NULL
        CHECK (length(CAST(relative_path AS BLOB)) BETWEEN 1 AND 16384),
    location_id INTEGER REFERENCES object_locations(location_id),
    object_id TEXT REFERENCES content_objects(object_id),
    old_pack_id TEXT REFERENCES packs(pack_id),
    expected_checksum TEXT CHECK (
        expected_checksum IS NULL OR (
            length(expected_checksum) = 67
            AND substr(expected_checksum, 1, 3) = 'b3:'
            AND substr(expected_checksum, 4) NOT GLOB '*[^0-9a-f]*'
        )
    ),
    expected_file_bytes INTEGER NOT NULL CHECK (expected_file_bytes >= 0),
    state TEXT NOT NULL CHECK (state IN ('pending', 'unlinking', 'retired')),
    observed_file_bytes INTEGER CHECK (observed_file_bytes >= 0),
    prepared_at INTEGER NOT NULL CHECK (prepared_at >= 0),
    unlink_started_at INTEGER CHECK (unlink_started_at >= 0),
    retired_at INTEGER CHECK (retired_at >= 0),
    PRIMARY KEY (purge_id, file_kind, relative_path),
    UNIQUE (purge_id, location_id),
    FOREIGN KEY (purge_id, old_pack_id)
        REFERENCES purge_pack_work(purge_id, old_pack_id),
    CHECK (
        (file_kind = 'loose'
            AND location_id IS NOT NULL
            AND object_id IS NOT NULL
            AND old_pack_id IS NULL
            AND expected_checksum IS NULL)
        OR (file_kind IN ('pack', 'index')
            AND location_id IS NULL
            AND object_id IS NULL
            AND old_pack_id IS NOT NULL
            AND expected_checksum IS NOT NULL)
    ),
    CHECK (
        (state = 'pending'
            AND observed_file_bytes IS NULL
            AND unlink_started_at IS NULL
            AND retired_at IS NULL)
        OR (state = 'unlinking'
            AND observed_file_bytes IS NOT NULL
            AND unlink_started_at IS NOT NULL
            AND retired_at IS NULL)
        OR (state = 'retired'
            AND observed_file_bytes IS NOT NULL
            AND unlink_started_at IS NOT NULL
            AND retired_at IS NOT NULL)
    )
) STRICT;

CREATE INDEX purge_file_work_next
    ON purge_file_work(purge_id, state, file_kind, relative_path);

CREATE INDEX purge_file_work_by_pack
    ON purge_file_work(purge_id, old_pack_id, file_kind);

CREATE TABLE purge_replacement_metrics (
    purge_id INTEGER NOT NULL,
    old_pack_id TEXT NOT NULL,
    replacement_pack_id TEXT NOT NULL REFERENCES packs(pack_id),
    pack_bytes INTEGER NOT NULL CHECK (pack_bytes >= 0),
    index_bytes INTEGER NOT NULL CHECK (index_bytes >= 0),
    activated_at INTEGER NOT NULL CHECK (activated_at >= 0),
    PRIMARY KEY (purge_id, old_pack_id),
    UNIQUE (purge_id, replacement_pack_id),
    FOREIGN KEY (purge_id, old_pack_id)
        REFERENCES purge_pack_work(purge_id, old_pack_id)
) STRICT;

CREATE TABLE purge_cleanup_accounting (
    purge_id INTEGER PRIMARY KEY
        REFERENCES purge_operations(purge_id) ON DELETE CASCADE,
    retired_file_bytes INTEGER NOT NULL DEFAULT 0 CHECK (retired_file_bytes >= 0),
    replacement_file_bytes INTEGER NOT NULL DEFAULT 0
        CHECK (replacement_file_bytes >= 0),
    directories_synced_at INTEGER CHECK (directories_synced_at >= 0),
    completed_at INTEGER CHECK (completed_at >= 0),
    CHECK (
        (directories_synced_at IS NULL AND completed_at IS NULL)
        OR (directories_synced_at IS NOT NULL AND completed_at IS NOT NULL)
    )
) STRICT;
