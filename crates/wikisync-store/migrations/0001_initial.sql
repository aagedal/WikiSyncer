CREATE TABLE content_objects (
    object_id TEXT PRIMARY KEY NOT NULL,
    object_kind TEXT NOT NULL CHECK (object_kind IN ('wikitext', 'media')),
    uncompressed_length INTEGER NOT NULL CHECK (uncompressed_length >= 0),
    media_type TEXT NOT NULL,
    verification_state TEXT NOT NULL DEFAULT 'verified'
        CHECK (verification_state IN ('pending', 'verified', 'corrupt')),
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE object_locations (
    location_id INTEGER PRIMARY KEY,
    object_id TEXT NOT NULL REFERENCES content_objects(object_id) ON DELETE CASCADE,
    storage_kind TEXT NOT NULL CHECK (storage_kind IN ('loose', 'pack')),
    encoding TEXT NOT NULL CHECK (encoding IN ('zstd', 'pack-full', 'pack-delta')),
    relative_path TEXT NOT NULL,
    compressed_length INTEGER NOT NULL CHECK (compressed_length >= 0),
    base_object_id TEXT REFERENCES content_objects(object_id),
    pack_generation INTEGER,
    verification_state TEXT NOT NULL DEFAULT 'verified'
        CHECK (verification_state IN ('pending', 'verified', 'obsolete', 'corrupt')),
    created_at INTEGER NOT NULL,
    UNIQUE (object_id, storage_kind, relative_path),
    CHECK (
        (storage_kind = 'loose' AND encoding = 'zstd'
            AND base_object_id IS NULL AND pack_generation IS NULL)
        OR (storage_kind = 'pack'
            AND encoding IN ('pack-full', 'pack-delta')
            AND pack_generation IS NOT NULL
            AND (
                (encoding = 'pack-full' AND base_object_id IS NULL)
                OR (encoding = 'pack-delta' AND base_object_id IS NOT NULL)
            ))
    )
) STRICT;

CREATE INDEX object_locations_lookup
    ON object_locations(object_id, verification_state, storage_kind);
