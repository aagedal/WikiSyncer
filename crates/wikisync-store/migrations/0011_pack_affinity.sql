CREATE INDEX revisions_by_content_affinity
    ON revisions(content_object_id, wiki_id, page_id, revision_id);
