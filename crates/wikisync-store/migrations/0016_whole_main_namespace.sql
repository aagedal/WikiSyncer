CREATE TABLE collection_configuration_v16 (
    collection_id INTEGER PRIMARY KEY
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    rule_kind TEXT NOT NULL
        CHECK (rule_kind IN (
            'whole-main-namespace', 'explicit-titles', 'title-list', 'category'
        )),
    category_title TEXT,
    category_recursion_depth INTEGER
        CHECK (category_recursion_depth BETWEEN 0 AND 65535),
    history_kind TEXT NOT NULL
        CHECK (history_kind IN ('current-and-future', 'last-n', 'since', 'complete')),
    history_value INTEGER,
    maximum_pages INTEGER CHECK (maximum_pages > 0),
    maximum_bytes INTEGER CHECK (maximum_bytes > 0),
    removal_policy TEXT NOT NULL
        CHECK (removal_policy IN ('stop-tracking-retain-history', 'keep-tracking')),
    updated_at INTEGER NOT NULL,
    image_policy TEXT NOT NULL DEFAULT 'none'
        CHECK (image_policy IN ('none', 'thumbnails')),
    thumbnail_max_edge_pixels INTEGER
        CHECK (thumbnail_max_edge_pixels BETWEEN 1 AND 4096),
    thumbnail_max_images_per_revision INTEGER
        CHECK (thumbnail_max_images_per_revision BETWEEN 1 AND 256),
    thumbnail_max_bytes_per_image INTEGER
        CHECK (thumbnail_max_bytes_per_image BETWEEN 1 AND 67108864),
    CHECK (
        (rule_kind IN ('whole-main-namespace', 'explicit-titles', 'title-list')
            AND category_title IS NULL AND category_recursion_depth IS NULL)
        OR (rule_kind = 'category'
            AND category_title IS NOT NULL AND category_recursion_depth IS NOT NULL)
    ),
    CHECK (
        (rule_kind != 'whole-main-namespace')
        OR (history_kind = 'current-and-future' AND history_value IS NULL)
    ),
    CHECK (
        (history_kind = 'last-n' AND history_value > 0)
        OR (history_kind = 'since' AND history_value IS NOT NULL)
        OR (history_kind IN ('current-and-future', 'complete') AND history_value IS NULL)
    )
) STRICT;

INSERT INTO collection_configuration_v16 (
    collection_id, rule_kind, category_title, category_recursion_depth,
    history_kind, history_value, maximum_pages, maximum_bytes,
    removal_policy, updated_at, image_policy, thumbnail_max_edge_pixels,
    thumbnail_max_images_per_revision, thumbnail_max_bytes_per_image
)
SELECT collection_id, rule_kind, category_title, category_recursion_depth,
       history_kind, history_value, maximum_pages, maximum_bytes,
       removal_policy, updated_at, image_policy, thumbnail_max_edge_pixels,
       thumbnail_max_images_per_revision, thumbnail_max_bytes_per_image
FROM collection_configuration;

CREATE TABLE collection_rule_titles_v16 (
    collection_id INTEGER NOT NULL
        REFERENCES collection_configuration_v16(collection_id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    PRIMARY KEY (collection_id, title)
) STRICT;

INSERT INTO collection_rule_titles_v16 (collection_id, title)
SELECT collection_id, title FROM collection_rule_titles;

DROP TABLE collection_rule_titles;
DROP TABLE collection_configuration;
ALTER TABLE collection_configuration_v16 RENAME TO collection_configuration;
ALTER TABLE collection_rule_titles_v16 RENAME TO collection_rule_titles;

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

CREATE TABLE collection_resolved_members_v16 (
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    page_id INTEGER NOT NULL CHECK (page_id > 0),
    namespace INTEGER NOT NULL,
    title TEXT NOT NULL,
    inclusion_kind TEXT NOT NULL
        CHECK (inclusion_kind IN (
            'whole-main-namespace', 'explicit-title', 'title-list', 'category'
        )),
    inclusion_title TEXT NOT NULL,
    inclusion_depth INTEGER CHECK (inclusion_depth BETWEEN 0 AND 65535),
    membership_state TEXT NOT NULL
        CHECK (membership_state IN ('active', 'removed')),
    first_resolved_at INTEGER NOT NULL,
    last_resolved_at INTEGER NOT NULL,
    removed_at INTEGER,
    PRIMARY KEY (collection_id, page_id),
    CHECK (
        (inclusion_kind = 'category' AND inclusion_depth IS NOT NULL)
        OR (inclusion_kind IN (
                'whole-main-namespace', 'explicit-title', 'title-list'
            ) AND inclusion_depth IS NULL)
    ),
    CHECK (
        (inclusion_kind != 'whole-main-namespace') OR namespace = 0
    ),
    CHECK (
        (membership_state = 'active' AND removed_at IS NULL)
        OR (membership_state = 'removed' AND removed_at IS NOT NULL)
    )
) STRICT;

INSERT INTO collection_resolved_members_v16 (
    collection_id, wiki_id, page_id, namespace, title, inclusion_kind,
    inclusion_title, inclusion_depth, membership_state, first_resolved_at,
    last_resolved_at, removed_at
)
SELECT collection_id, wiki_id, page_id, namespace, title, inclusion_kind,
       inclusion_title, inclusion_depth, membership_state, first_resolved_at,
       last_resolved_at, removed_at
FROM collection_resolved_members;

DROP TABLE collection_resolved_members;
ALTER TABLE collection_resolved_members_v16 RENAME TO collection_resolved_members;

CREATE INDEX collection_resolved_members_active
    ON collection_resolved_members(collection_id, membership_state, page_id);
CREATE INDEX collection_resolved_members_by_page
    ON collection_resolved_members(wiki_id, page_id, collection_id);

CREATE TABLE collection_pages_v16 (
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    wiki_id INTEGER NOT NULL,
    page_id INTEGER NOT NULL,
    inclusion_reason TEXT NOT NULL
        CHECK (inclusion_reason IN ('explicit-title', 'whole-main-namespace')),
    added_at INTEGER NOT NULL,
    PRIMARY KEY (collection_id, wiki_id, page_id),
    FOREIGN KEY (wiki_id, page_id) REFERENCES pages(wiki_id, page_id)
) STRICT;

INSERT INTO collection_pages_v16 (
    collection_id, wiki_id, page_id, inclusion_reason, added_at
)
SELECT collection_id, wiki_id, page_id, inclusion_reason, added_at
FROM collection_pages;

DROP TABLE collection_pages;
ALTER TABLE collection_pages_v16 RENAME TO collection_pages;
