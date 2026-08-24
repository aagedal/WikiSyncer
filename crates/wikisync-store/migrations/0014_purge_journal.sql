CREATE TABLE purge_operations (
    purge_id INTEGER PRIMARY KEY CHECK (purge_id > 0),
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id),
    collection_name TEXT NOT NULL
        CHECK (length(CAST(collection_name AS BLOB)) BETWEEN 1 AND 16384),
    collection_generation INTEGER NOT NULL CHECK (collection_generation > 0),
    tombstoned_at INTEGER NOT NULL CHECK (tombstoned_at >= 0),
    manifest_head_sequence INTEGER CHECK (manifest_head_sequence > 0),
    manifest_head_id TEXT CHECK (
        manifest_head_id IS NULL OR (
            length(manifest_head_id) = 67
            AND substr(manifest_head_id, 1, 3) = 'b3:'
            AND substr(manifest_head_id, 4) NOT GLOB '*[^0-9a-f]*'
        )
    ),
    preview_fingerprint TEXT NOT NULL CHECK (
        length(preview_fingerprint) = 67
        AND substr(preview_fingerprint, 1, 3) = 'b3:'
        AND substr(preview_fingerprint, 4) NOT GLOB '*[^0-9a-f]*'
    ),
    object_count INTEGER NOT NULL CHECK (object_count > 0),
    wikitext_object_count INTEGER NOT NULL CHECK (wikitext_object_count >= 0),
    media_object_count INTEGER NOT NULL CHECK (media_object_count >= 0),
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0),
    reclaimable_bytes INTEGER NOT NULL CHECK (reclaimable_bytes >= 0),
    loose_object_count INTEGER NOT NULL CHECK (loose_object_count >= 0),
    affected_pack_count INTEGER NOT NULL CHECK (affected_pack_count >= 0),
    whole_pack_count INTEGER NOT NULL CHECK (whole_pack_count >= 0),
    mixed_pack_count INTEGER NOT NULL CHECK (mixed_pack_count >= 0),
    state TEXT NOT NULL CHECK (
        state IN ('authorized', 'repacking', 'cleaning', 'succeeded', 'failed')
    ),
    acknowledged_collection_name TEXT NOT NULL,
    acknowledged_preview_fingerprint TEXT NOT NULL,
    payload_only_acknowledged INTEGER NOT NULL CHECK (payload_only_acknowledged = 1),
    backups_not_erased_acknowledged INTEGER NOT NULL
        CHECK (backups_not_erased_acknowledged = 1),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    authorized_at INTEGER NOT NULL CHECK (authorized_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
    finished_at INTEGER,
    CHECK (
        (manifest_head_sequence IS NULL AND manifest_head_id IS NULL)
        OR (manifest_head_sequence IS NOT NULL AND manifest_head_id IS NOT NULL)
    ),
    CHECK (object_count = wikitext_object_count + media_object_count),
    CHECK (affected_pack_count = whole_pack_count + mixed_pack_count),
    CHECK (acknowledged_collection_name = collection_name),
    CHECK (acknowledged_preview_fingerprint = preview_fingerprint),
    CHECK (
        (state IN ('authorized', 'repacking', 'cleaning') AND finished_at IS NULL)
        OR (state IN ('succeeded', 'failed') AND finished_at IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX one_unfinished_purge_per_collection
    ON purge_operations(collection_id)
    WHERE state IN ('authorized', 'repacking', 'cleaning');

CREATE TABLE purge_objects (
    purge_id INTEGER NOT NULL
        REFERENCES purge_operations(purge_id) ON DELETE CASCADE,
    object_id TEXT NOT NULL REFERENCES content_objects(object_id),
    object_kind TEXT NOT NULL CHECK (object_kind IN ('wikitext', 'media')),
    uncompressed_length INTEGER NOT NULL CHECK (uncompressed_length >= 0),
    PRIMARY KEY (purge_id, object_id)
) STRICT;

CREATE INDEX purge_objects_by_object
    ON purge_objects(object_id, purge_id);

CREATE INDEX collection_resolved_members_by_page
    ON collection_resolved_members(wiki_id, page_id, collection_id);

CREATE INDEX page_media_by_content_object
    ON page_media(content_object_id, wiki_id, revision_id);

CREATE TABLE purge_pack_work (
    purge_id INTEGER NOT NULL
        REFERENCES purge_operations(purge_id) ON DELETE CASCADE,
    old_pack_id TEXT NOT NULL REFERENCES packs(pack_id),
    purged_object_count INTEGER NOT NULL CHECK (purged_object_count > 0),
    retained_object_count INTEGER NOT NULL CHECK (retained_object_count >= 0),
    replacement_pack_id TEXT REFERENCES packs(pack_id),
    state TEXT NOT NULL CHECK (state IN ('pending', 'replacement-ready', 'retired')),
    PRIMARY KEY (purge_id, old_pack_id),
    CHECK (
        (state = 'pending' AND replacement_pack_id IS NULL)
        OR (state IN ('replacement-ready', 'retired')
            AND (retained_object_count = 0 OR replacement_pack_id IS NOT NULL))
    )
) STRICT;
