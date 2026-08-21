CREATE TABLE packs (
    pack_id TEXT PRIMARY KEY NOT NULL,
    generation INTEGER NOT NULL UNIQUE CHECK (generation > 0),
    pack_path TEXT NOT NULL UNIQUE,
    index_path TEXT NOT NULL UNIQUE,
    pack_checksum TEXT NOT NULL,
    index_checksum TEXT NOT NULL,
    object_count INTEGER NOT NULL CHECK (object_count > 0),
    state TEXT NOT NULL CHECK (state IN ('verified', 'obsolete', 'corrupt')),
    created_at INTEGER NOT NULL,
    verified_at INTEGER NOT NULL
) STRICT;

ALTER TABLE object_locations ADD COLUMN pack_id TEXT REFERENCES packs(pack_id);
ALTER TABLE object_locations ADD COLUMN pack_offset INTEGER CHECK (pack_offset >= 0);
ALTER TABLE object_locations ADD COLUMN delta_depth INTEGER CHECK (delta_depth >= 0);

CREATE INDEX object_locations_by_pack
    ON object_locations(pack_id, pack_offset);

CREATE TRIGGER object_locations_pack_fields_insert
BEFORE INSERT ON object_locations
BEGIN
    SELECT CASE
        WHEN NEW.storage_kind = 'loose'
             AND (NEW.pack_id IS NOT NULL OR NEW.pack_offset IS NOT NULL
                  OR NEW.delta_depth IS NOT NULL)
        THEN RAISE(ABORT, 'loose location has pack fields')
        WHEN NEW.storage_kind = 'pack'
             AND (NEW.pack_id IS NULL OR NEW.pack_offset IS NULL
                  OR NEW.delta_depth IS NULL)
        THEN RAISE(ABORT, 'pack location lacks pack fields')
    END;
END;

CREATE TRIGGER object_locations_pack_fields_update
BEFORE UPDATE ON object_locations
BEGIN
    SELECT CASE
        WHEN NEW.storage_kind = 'loose'
             AND (NEW.pack_id IS NOT NULL OR NEW.pack_offset IS NOT NULL
                  OR NEW.delta_depth IS NOT NULL)
        THEN RAISE(ABORT, 'loose location has pack fields')
        WHEN NEW.storage_kind = 'pack'
             AND (NEW.pack_id IS NULL OR NEW.pack_offset IS NULL
                  OR NEW.delta_depth IS NULL)
        THEN RAISE(ABORT, 'pack location lacks pack fields')
    END;
END;
