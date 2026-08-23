ALTER TABLE collection_configuration
    ADD COLUMN image_policy TEXT NOT NULL DEFAULT 'none'
        CHECK (image_policy IN ('none', 'thumbnails'));
ALTER TABLE collection_configuration
    ADD COLUMN thumbnail_max_edge_pixels INTEGER
        CHECK (thumbnail_max_edge_pixels BETWEEN 1 AND 4096);
ALTER TABLE collection_configuration
    ADD COLUMN thumbnail_max_images_per_revision INTEGER
        CHECK (thumbnail_max_images_per_revision BETWEEN 1 AND 256);
ALTER TABLE collection_configuration
    ADD COLUMN thumbnail_max_bytes_per_image INTEGER
        CHECK (thumbnail_max_bytes_per_image BETWEEN 1 AND 67108864);

CREATE TRIGGER collection_configuration_image_policy_insert
BEFORE INSERT ON collection_configuration
BEGIN
    SELECT CASE
        WHEN NEW.image_policy = 'none'
             AND (NEW.thumbnail_max_edge_pixels IS NOT NULL
                  OR NEW.thumbnail_max_images_per_revision IS NOT NULL
                  OR NEW.thumbnail_max_bytes_per_image IS NOT NULL)
        THEN RAISE(ABORT, 'disabled image policy has thumbnail bounds')
        WHEN NEW.image_policy = 'thumbnails'
             AND (NEW.thumbnail_max_edge_pixels IS NULL
                  OR NEW.thumbnail_max_images_per_revision IS NULL
                  OR NEW.thumbnail_max_bytes_per_image IS NULL)
        THEN RAISE(ABORT, 'thumbnail image policy lacks bounds')
    END;
END;

CREATE TRIGGER collection_configuration_image_policy_update
BEFORE UPDATE ON collection_configuration
BEGIN
    SELECT CASE
        WHEN NEW.image_policy = 'none'
             AND (NEW.thumbnail_max_edge_pixels IS NOT NULL
                  OR NEW.thumbnail_max_images_per_revision IS NOT NULL
                  OR NEW.thumbnail_max_bytes_per_image IS NOT NULL)
        THEN RAISE(ABORT, 'disabled image policy has thumbnail bounds')
        WHEN NEW.image_policy = 'thumbnails'
             AND (NEW.thumbnail_max_edge_pixels IS NULL
                  OR NEW.thumbnail_max_images_per_revision IS NULL
                  OR NEW.thumbnail_max_bytes_per_image IS NULL)
        THEN RAISE(ABORT, 'thumbnail image policy lacks bounds')
    END;
END;

CREATE TABLE media (
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    source_media_id INTEGER NOT NULL CHECK (source_media_id > 0),
    source_sha1 TEXT NOT NULL
        CHECK (length(CAST(source_sha1 AS BLOB)) BETWEEN 1 AND 128),
    file_title TEXT NOT NULL
        CHECK (length(CAST(file_title AS BLOB)) BETWEEN 1 AND 16384),
    original_url TEXT NOT NULL
        CHECK (length(CAST(original_url AS BLOB)) BETWEEN 1 AND 16384),
    description_url TEXT NOT NULL
        CHECK (length(CAST(description_url AS BLOB)) BETWEEN 1 AND 16384),
    author TEXT NOT NULL
        CHECK (length(CAST(author AS BLOB)) BETWEEN 1 AND 16384),
    attribution TEXT NOT NULL
        CHECK (length(CAST(attribution AS BLOB)) BETWEEN 1 AND 16384),
    license_name TEXT NOT NULL
        CHECK (length(CAST(license_name AS BLOB)) BETWEEN 1 AND 16384),
    license_url TEXT
        CHECK (license_url IS NULL
               OR length(CAST(license_url AS BLOB)) BETWEEN 1 AND 16384),
    width INTEGER NOT NULL CHECK (width BETWEEN 1 AND 4096),
    height INTEGER NOT NULL CHECK (height BETWEEN 1 AND 4096),
    mime_type TEXT NOT NULL CHECK (mime_type IN ('image/jpeg', 'image/png')),
    captured_at INTEGER NOT NULL CHECK (captured_at >= 0),
    content_object_id TEXT NOT NULL REFERENCES content_objects(object_id),
    PRIMARY KEY (wiki_id, source_media_id, source_sha1, content_object_id)
) STRICT;

CREATE INDEX media_by_content_object
    ON media(content_object_id, wiki_id, source_media_id);

CREATE TRIGGER media_object_kind_insert
BEFORE INSERT ON media
BEGIN
    SELECT CASE
        WHEN (SELECT object_kind FROM content_objects
              WHERE object_id = NEW.content_object_id) != 'media'
        THEN RAISE(ABORT, 'media metadata references a non-media object')
    END;
END;

CREATE TRIGGER media_object_kind_update
BEFORE UPDATE OF content_object_id ON media
BEGIN
    SELECT CASE
        WHEN (SELECT object_kind FROM content_objects
              WHERE object_id = NEW.content_object_id) != 'media'
        THEN RAISE(ABORT, 'media metadata references a non-media object')
    END;
END;

CREATE TABLE page_media (
    wiki_id INTEGER NOT NULL,
    revision_id INTEGER NOT NULL CHECK (revision_id > 0),
    placement_index INTEGER NOT NULL CHECK (placement_index BETWEEN 0 AND 255),
    source_media_id INTEGER NOT NULL CHECK (source_media_id > 0),
    source_sha1 TEXT NOT NULL
        CHECK (length(CAST(source_sha1 AS BLOB)) BETWEEN 1 AND 128),
    content_object_id TEXT NOT NULL REFERENCES content_objects(object_id),
    placement_kind TEXT NOT NULL CHECK (placement_kind IN ('lead', 'inline')),
    caption TEXT CHECK (
        caption IS NULL OR length(CAST(caption AS BLOB)) BETWEEN 1 AND 16384
    ),
    alt_text TEXT CHECK (
        alt_text IS NULL OR length(CAST(alt_text AS BLOB)) BETWEEN 1 AND 16384
    ),
    PRIMARY KEY (wiki_id, revision_id, placement_index),
    FOREIGN KEY (wiki_id, revision_id) REFERENCES revisions(wiki_id, revision_id),
    FOREIGN KEY (wiki_id, source_media_id, source_sha1, content_object_id)
        REFERENCES media(wiki_id, source_media_id, source_sha1, content_object_id)
) STRICT;

CREATE INDEX page_media_by_media
    ON page_media(
        wiki_id, source_media_id, source_sha1, content_object_id, revision_id
    );
