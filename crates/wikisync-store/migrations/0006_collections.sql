CREATE TABLE collection_configuration (
    collection_id INTEGER PRIMARY KEY
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    rule_kind TEXT NOT NULL
        CHECK (rule_kind IN ('explicit-titles', 'title-list', 'category')),
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
    CHECK (
        (rule_kind IN ('explicit-titles', 'title-list')
            AND category_title IS NULL AND category_recursion_depth IS NULL)
        OR (rule_kind = 'category'
            AND category_title IS NOT NULL AND category_recursion_depth IS NOT NULL)
    ),
    CHECK (
        (history_kind = 'last-n' AND history_value > 0)
        OR (history_kind = 'since' AND history_value IS NOT NULL)
        OR (history_kind IN ('current-and-future', 'complete') AND history_value IS NULL)
    )
) STRICT;

CREATE TABLE collection_rule_titles (
    collection_id INTEGER NOT NULL
        REFERENCES collection_configuration(collection_id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    PRIMARY KEY (collection_id, title)
) STRICT;

CREATE TABLE collection_resolved_members (
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    wiki_id INTEGER NOT NULL REFERENCES wikis(wiki_id),
    page_id INTEGER NOT NULL CHECK (page_id > 0),
    namespace INTEGER NOT NULL,
    title TEXT NOT NULL,
    inclusion_kind TEXT NOT NULL
        CHECK (inclusion_kind IN ('explicit-title', 'title-list', 'category')),
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
        OR (inclusion_kind IN ('explicit-title', 'title-list')
            AND inclusion_depth IS NULL)
    ),
    CHECK (
        (membership_state = 'active' AND removed_at IS NULL)
        OR (membership_state = 'removed' AND removed_at IS NOT NULL)
    )
) STRICT;

CREATE INDEX collection_resolved_members_active
    ON collection_resolved_members(collection_id, membership_state, page_id);

CREATE TABLE collection_estimates (
    estimate_id INTEGER PRIMARY KEY CHECK (estimate_id > 0),
    collection_id INTEGER NOT NULL
        REFERENCES collections(collection_id) ON DELETE CASCADE,
    resolved_page_count INTEGER NOT NULL CHECK (resolved_page_count >= 0),
    predicted_canonical_bytes INTEGER CHECK (predicted_canonical_bytes >= 0),
    estimated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX collection_estimates_latest
    ON collection_estimates(collection_id, estimated_at DESC, estimate_id DESC);

INSERT INTO collection_configuration (
    collection_id, rule_kind, category_title, category_recursion_depth,
    history_kind, history_value, maximum_pages, maximum_bytes,
    removal_policy, updated_at
)
SELECT collection_id, 'explicit-titles', NULL, NULL,
       'current-and-future', NULL, NULL, NULL,
       'stop-tracking-retain-history', created_at
FROM collections;

INSERT INTO collection_resolved_members (
    collection_id, wiki_id, page_id, namespace, title,
    inclusion_kind, inclusion_title, inclusion_depth, membership_state,
    first_resolved_at, last_resolved_at, removed_at
)
SELECT collection_pages.collection_id, collection_pages.wiki_id,
       collection_pages.page_id, pages.namespace, pages.current_title,
       'explicit-title', pages.current_title, NULL, 'active',
       collection_pages.added_at, collection_pages.added_at, NULL
FROM collection_pages
JOIN pages USING (wiki_id, page_id);
