mod support;

use support::{FixtureResponse, FixtureServer};
use wikisync_core::{PageId, PageTitle, RevisionId, TitleSelection};
use wikisync_mediawiki::{ClientConfig, MediaWikiClient};
use wikisync_search::{SearchIndex, SearchQuery, SqliteSearchIndex};
use wikisync_store::{Library, SyncRunKind, SyncRunState};
use wikisync_sync::{
    CategoryPreviewError, CategoryPreviewLimits, ReconciliationLimits, capture_explicit_titles,
    capture_revision_history, preview_category_selection, reconcile_collection_heads,
    reconcile_collection_heads_with_limits,
};

const TITLE_RESOLUTION: &str = include_str!("../../../fixtures/mediawiki/title-resolution.json");
const REVISION_CONTENT: &str = include_str!("../../../fixtures/mediawiki/revision-content.json");
const REVISIONS_PAGE_1: &str = include_str!("../../../fixtures/mediawiki/revisions-page-1.json");
const REVISIONS_PAGE_2: &str = include_str!("../../../fixtures/mediawiki/revisions-page-2.json");
const REVISION_CONTENT_OLDER: &str =
    include_str!("../../../fixtures/mediawiki/revision-content-older.json");
const CATEGORY_MEMBERS_PAGE_1: &str =
    include_str!("../../../fixtures/mediawiki/category-members-page-1.json");
const CATEGORY_MEMBERS_PAGE_2: &str =
    include_str!("../../../fixtures/mediawiki/category-members-page-2.json");
const CATEGORY_MEMBERS_SUBCATEGORY: &str =
    include_str!("../../../fixtures/mediawiki/category-members-subcategory.json");
const RECONCILIATION_TITLE_RESOLUTION: &str =
    include_str!("../../../fixtures/mediawiki/reconciliation-title-resolution.json");
const RECONCILIATION_REVISIONS: &str =
    include_str!("../../../fixtures/mediawiki/reconciliation-revisions.json");
const RECONCILIATION_CONTENT_MIDDLE: &str =
    include_str!("../../../fixtures/mediawiki/reconciliation-content-middle.json");
const RECONCILIATION_CONTENT_HEAD: &str =
    include_str!("../../../fixtures/mediawiki/reconciliation-content-head.json");
const RECONCILIATION_UNCHANGED_TITLE_RESOLUTION: &str =
    include_str!("../../../fixtures/mediawiki/reconciliation-unchanged-title-resolution.json");
const RECONCILIATION_MISSING_PAGE: &str =
    include_str!("../../../fixtures/mediawiki/reconciliation-missing-page.json");
const RECONCILIATION_REVISIONS_FROM_MIDDLE: &str =
    include_str!("../../../fixtures/mediawiki/reconciliation-revisions-from-middle.json");

#[tokio::test(flavor = "multi_thread")]
async fn category_preview_handles_continuation_recursion_cycles_and_deduplication() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(CATEGORY_MEMBERS_PAGE_1),
        FixtureResponse::json(CATEGORY_MEMBERS_PAGE_2),
        FixtureResponse::json(CATEGORY_MEMBERS_SUBCATEGORY),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 category-preview-test")
            .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    library
        .create_explicit_collection(wiki_id, "Existing collection")
        .expect("collection");
    let before = library.collections().expect("collections");

    let preview = preview_category_selection(
        &client,
        &PageTitle::new("Category:Root").expect("category"),
        1,
        CategoryPreviewLimits::default(),
    )
    .await
    .expect("category preview");

    assert_eq!(preview.batches, 3);
    assert_eq!(preview.categories.len(), 2);
    assert_eq!(preview.categories[0].title.as_str(), "Category:Root");
    assert_eq!(preview.categories[0].depth, 0);
    assert_eq!(preview.categories[1].title.as_str(), "Category:Subtopic");
    assert_eq!(preview.categories[1].depth, 1);
    assert_eq!(
        preview
            .pages
            .iter()
            .map(|page| page.title.as_str())
            .collect::<Vec<_>>(),
        ["Alpha", "Beta", "Gamma"]
    );
    assert!(preview.pages.iter().all(|page| page.namespace == 0));
    assert_eq!(library.collections().expect("collections"), before);
    assert!(
        library
            .pages_by_title(&PageTitle::new("Alpha").unwrap(), None)
            .unwrap()
            .is_empty()
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("cmcontinue="));
    assert!(requests[2].contains("cmtitle=Category%3ASubtopic"));
}

#[tokio::test(flavor = "multi_thread")]
async fn category_preview_enforces_depth_and_page_bounds() {
    let empty_server = FixtureServer::start(vec![]);
    let client = MediaWikiClient::new(
        ClientConfig::new(
            empty_server.endpoint(),
            "WikiSyncer/0.1 category-bounds-test",
        )
        .expect("client configuration"),
    )
    .expect("client");
    let root = PageTitle::new("Category:Root").expect("category");
    let error = preview_category_selection(&client, &root, 17, CategoryPreviewLimits::default())
        .await
        .expect_err("depth must be bounded before network access");
    assert!(matches!(
        error,
        CategoryPreviewError::DepthLimitExceeded {
            requested: 17,
            limit: 16
        }
    ));
    empty_server.finish();

    let server = FixtureServer::start(vec![
        FixtureResponse::json(CATEGORY_MEMBERS_PAGE_1),
        FixtureResponse::json(CATEGORY_MEMBERS_PAGE_2),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 category-bounds-test")
            .expect("client configuration"),
    )
    .expect("client");
    let error = preview_category_selection(
        &client,
        &root,
        0,
        CategoryPreviewLimits {
            max_pages: 1,
            ..CategoryPreviewLimits::default()
        },
    )
    .await
    .expect_err("unique page limit");
    assert!(matches!(
        error,
        CategoryPreviewError::PageLimitExceeded { limit: 1 }
    ));
    server.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_title_capture_is_durable_and_idempotent() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
    ]);
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 capture-test")
        .expect("client configuration");
    let client = MediaWikiClient::new(config).expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Fixture pages")
        .expect("collection");
    let selection = TitleSelection::new([
        PageTitle::new("Rust_programming_language").expect("title"),
        PageTitle::new("Definitely missing WikiSyncer fixture page").expect("title"),
    ])
    .expect("selection");

    let first = capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
        .await
        .expect("first capture");
    assert_eq!(first.pages.len(), 1);
    assert!(first.pages[0].newly_captured);
    assert_eq!(first.missing_titles.len(), 1);
    assert_eq!(
        library
            .unresolved_titles(collection_id)
            .expect("unresolved titles"),
        first.missing_titles
    );

    let page_id = PageId::new(25_357_340).expect("page ID");
    let page = library
        .page(wiki_id, page_id)
        .expect("page lookup")
        .expect("captured page");
    assert_eq!(page.title.as_str(), "Rust (programming language)");
    let revision = library
        .revision(wiki_id, first.pages[0].revision_id)
        .expect("revision lookup")
        .expect("captured revision");
    assert_eq!(
        library
            .read_object(revision.content_object_id)
            .expect("canonical source"),
        b"== Rust ==\nA systems programming language."
    );
    let search = SqliteSearchIndex::open(&library).expect("search index");
    let hits = search
        .search(SearchQuery::new("systems programming"))
        .expect("offline search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page_id, page_id);

    let second = capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
        .await
        .expect("repeat capture");
    assert_eq!(second.pages.len(), 1);
    assert!(!second.pages[0].newly_captured);
    assert_eq!(
        second.pages[0].content_object_id,
        first.pages[0].content_object_id
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].contains("titles=Definitely+missing"));
    assert!(requests[1].contains("rvstartid=1300000001"));
}

#[tokio::test(flavor = "multi_thread")]
async fn revision_history_enumeration_reuses_the_head_and_captures_older_content() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(REVISIONS_PAGE_1),
        FixtureResponse::json(REVISIONS_PAGE_2),
        FixtureResponse::json(REVISION_CONTENT_OLDER),
    ]);
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 history-test")
        .expect("client configuration");
    let client = MediaWikiClient::new(config).expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Fixture pages")
        .expect("collection");
    let selection =
        TitleSelection::new([PageTitle::new("Rust_programming_language").expect("title")])
            .expect("selection");
    let current =
        capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
            .await
            .expect("current capture");
    let page_id = current.pages[0].page_id;

    let report = capture_revision_history(&client, &mut library, wiki_id, page_id)
        .await
        .expect("history capture");
    assert_eq!(report.batches, 2);
    assert_eq!(report.revisions_enumerated, 2);
    assert_eq!(report.revisions_reused, 1);
    assert_eq!(report.revisions_captured, 1);
    let history = library
        .revisions_for_page(wiki_id, page_id)
        .expect("stored history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].revision_id, current.pages[0].revision_id);
    assert_eq!(history[1].revision_id.get(), 1_300_000_000);
    assert_eq!(
        library
            .read_object(history[1].content_object_id)
            .expect("older source"),
        b"== Rust ==\nA programming language."
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 5);
    assert!(requests[2].contains("rvdir=older"));
    assert!(requests[3].contains("rvcontinue="));
    assert!(requests[4].contains("rvstartid=1300000000"));
}

#[tokio::test(flavor = "multi_thread")]
async fn long_gap_reconciliation_captures_every_intermediate_before_advancing_checkpoint() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(RECONCILIATION_TITLE_RESOLUTION),
        FixtureResponse::json(RECONCILIATION_REVISIONS),
        FixtureResponse::json(RECONCILIATION_CONTENT_MIDDLE),
        FixtureResponse::json(RECONCILIATION_CONTENT_HEAD),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 reconciliation-test")
            .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Fixture pages")
        .expect("collection");
    let selection =
        TitleSelection::new([PageTitle::new("Rust_programming_language").expect("title")])
            .expect("selection");
    let initial =
        capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
            .await
            .expect("initial capture");

    let report =
        reconcile_collection_heads(&client, &mut library, wiki_id, collection_id, 1_776_945_600)
            .await
            .expect("reconciliation");
    assert_eq!(report.status.state, SyncRunState::Succeeded);
    assert!(!report.resumed);
    assert_eq!(report.pages_checked, 1);
    assert_eq!(report.differing_heads, 1);
    assert_eq!(report.revision_batches, 1);
    assert_eq!(report.revisions_enumerated, 2);
    assert_eq!(report.revisions_captured, 2);
    assert_eq!(report.revisions_reused, 0);

    let page_id = initial.pages[0].page_id;
    let page = library
        .page(wiki_id, page_id)
        .expect("page lookup")
        .expect("page");
    assert_eq!(page.title.as_str(), "Rust language");
    assert_eq!(page.current_revision_id.expect("head").get(), 1_300_000_003);
    let history = library
        .revisions_for_page(wiki_id, page_id)
        .expect("history");
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].revision_id.get(), 1_300_000_003);
    assert_eq!(history[1].revision_id.get(), 1_300_000_002);
    assert_eq!(history[2].revision_id.get(), 1_300_000_001);
    let checkpoint = library.sync_checkpoints().expect("checkpoints").remove(0);
    assert_eq!(checkpoint.committed_through, 1_776_945_600);
    assert_eq!(checkpoint.reconciled_at, Some(1_776_945_600));
    assert_eq!(checkpoint.last_run_id, Some(report.status.run_id));
    let search = SqliteSearchIndex::open(&library).expect("search index");
    let hits = search
        .search(SearchQuery::new("safe concurrency"))
        .expect("updated search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].revision_id.get(), 1_300_000_003);

    let requests = server.finish();
    assert_eq!(requests.len(), 6);
    assert!(requests[2].contains("pageids=25357340"));
    assert!(requests[3].contains("rvdir=newer"));
    assert!(requests[3].contains("rvstartid=1300000001"));
    assert!(requests[4].contains("rvstartid=1300000002"));
    assert!(requests[5].contains("rvstartid=1300000003"));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_reconciliation_keeps_partial_content_but_not_head_or_checkpoint() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(RECONCILIATION_TITLE_RESOLUTION),
        FixtureResponse::json(RECONCILIATION_REVISIONS),
        FixtureResponse::json(RECONCILIATION_CONTENT_MIDDLE),
        // The head request deliberately receives the middle revision again.
        FixtureResponse::json(RECONCILIATION_CONTENT_MIDDLE),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(
            server.endpoint(),
            "WikiSyncer/0.1 reconciliation-failure-test",
        )
        .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Fixture pages")
        .expect("collection");
    let selection =
        TitleSelection::new([PageTitle::new("Rust_programming_language").expect("title")])
            .expect("selection");
    let initial =
        capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
            .await
            .expect("initial capture");
    let original_head = initial.pages[0].revision_id;

    reconcile_collection_heads(&client, &mut library, wiki_id, collection_id, 1_776_945_600)
        .await
        .expect_err("mismatched head content must fail reconciliation");

    assert!(
        library
            .revision(wiki_id, RevisionId::new(1_300_000_002).expect("revision"))
            .expect("middle revision lookup")
            .is_some(),
        "the bounded successful capture remains durable"
    );
    assert!(
        library
            .revision(wiki_id, RevisionId::new(1_300_000_003).expect("revision"))
            .expect("head revision lookup")
            .is_none()
    );
    assert_eq!(
        library
            .page(wiki_id, initial.pages[0].page_id)
            .expect("page lookup")
            .expect("page")
            .current_revision_id,
        Some(original_head)
    );
    let checkpoint = library.sync_checkpoints().expect("checkpoints").remove(0);
    assert_eq!(checkpoint.committed_through, 0);
    assert_eq!(checkpoint.reconciled_at, None);
    let run = library.sync_run_statuses(1).expect("run status").remove(0);
    assert_eq!(run.state, SyncRunState::Cancelled);
    assert_eq!(run.failed_jobs, 1);
    assert!(run.latest_error.is_some());

    let future = library
        .start_or_resume_sync_run(
            wiki_id,
            Some(collection_id),
            SyncRunKind::Reconciliation,
            1_776_945_700,
        )
        .expect("a non-retryable failure must not wedge the scope");
    assert_ne!(future.status.run_id, run.run_id);
    assert!(!future.resumed);
    library
        .cancel_sync_run(future.status.run_id)
        .expect("cancel follow-up run");

    let requests = server.finish();
    assert_eq!(requests.len(), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_unchanged_reconciliation_resumes_without_downloading_content() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(RECONCILIATION_UNCHANGED_TITLE_RESOLUTION),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(
            server.endpoint(),
            "WikiSyncer/0.1 reconciliation-resume-test",
        )
        .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Fixture pages")
        .expect("collection");
    let selection =
        TitleSelection::new([PageTitle::new("Rust_programming_language").expect("title")])
            .expect("selection");
    let initial =
        capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
            .await
            .expect("initial capture");
    let page_id = initial.pages[0].page_id;

    let interrupted = library
        .start_or_resume_sync_run(
            wiki_id,
            Some(collection_id),
            SyncRunKind::Reconciliation,
            1_776_945_600,
        )
        .expect("start interrupted run");
    let job = library
        .enqueue_sync_job(
            interrupted.status.run_id,
            &format!("reconcile-page:{page_id}"),
            "reconcile-page-head",
            Some(&page_id.to_string()),
        )
        .expect("enqueue interrupted job");
    assert_eq!(
        library
            .claim_next_sync_job(interrupted.status.run_id)
            .expect("claim interrupted job")
            .expect("job")
            .job_id,
        job.job_id
    );

    let report =
        reconcile_collection_heads(&client, &mut library, wiki_id, collection_id, 1_776_945_700)
            .await
            .expect("resumed reconciliation");
    assert!(report.resumed);
    assert_eq!(report.status.run_id, interrupted.status.run_id);
    assert_eq!(report.status.checkpoint_candidate, 1_776_945_600);
    assert_eq!(report.pages_checked, 1);
    assert_eq!(report.differing_heads, 0);
    assert_eq!(report.revision_batches, 0);
    assert_eq!(report.revisions_captured, 0);
    assert_eq!(report.status.state, SyncRunState::Succeeded);

    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].contains("prop=revisions"));
    assert!(!requests[2].contains("rvstartid="));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_page_is_reported_without_discarding_history_or_wedging_the_scope() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(RECONCILIATION_MISSING_PAGE),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(
            server.endpoint(),
            "WikiSyncer/0.1 reconciliation-missing-test",
        )
        .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Fixture pages")
        .expect("collection");
    let selection =
        TitleSelection::new([PageTitle::new("Rust_programming_language").expect("title")])
            .expect("selection");
    let initial =
        capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
            .await
            .expect("initial capture");

    let report =
        reconcile_collection_heads(&client, &mut library, wiki_id, collection_id, 1_776_945_600)
            .await
            .expect("missing page is a completed observation");
    assert_eq!(report.status.state, SyncRunState::Succeeded);
    assert_eq!(report.pages_checked, 1);
    assert_eq!(report.missing_pages, 1);
    assert_eq!(report.differing_heads, 0);
    assert_eq!(
        library
            .revisions_for_page(wiki_id, initial.pages[0].page_id)
            .expect("retained history")
            .len(),
        1
    );
    assert_eq!(
        library.sync_checkpoints().expect("checkpoint")[0].committed_through,
        1_776_945_600
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].contains("pageids=25357340"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reconciliation_limit_saves_progress_and_next_run_resumes_from_durable_tip() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(RECONCILIATION_TITLE_RESOLUTION),
        FixtureResponse::json(RECONCILIATION_REVISIONS),
        FixtureResponse::json(RECONCILIATION_CONTENT_MIDDLE),
        FixtureResponse::json(RECONCILIATION_TITLE_RESOLUTION),
        FixtureResponse::json(RECONCILIATION_REVISIONS_FROM_MIDDLE),
        FixtureResponse::json(RECONCILIATION_CONTENT_HEAD),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(
            server.endpoint(),
            "WikiSyncer/0.1 reconciliation-limit-test",
        )
        .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Fixture pages")
        .expect("collection");
    let selection =
        TitleSelection::new([PageTitle::new("Rust_programming_language").expect("title")])
            .expect("selection");
    let initial =
        capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
            .await
            .expect("initial capture");

    reconcile_collection_heads_with_limits(
        &client,
        &mut library,
        wiki_id,
        collection_id,
        1_776_945_600,
        ReconciliationLimits {
            max_batches_per_page: 1,
            max_revisions_per_page: 1,
        },
    )
    .await
    .expect_err("first run reaches the one-revision ceiling");
    assert_eq!(
        library.sync_run_statuses(1).expect("failed run")[0].state,
        SyncRunState::Cancelled
    );
    assert_eq!(
        library
            .newest_revision_for_page(wiki_id, initial.pages[0].page_id)
            .expect("durable tip")
            .expect("middle revision")
            .revision_id
            .get(),
        1_300_000_002
    );
    assert_eq!(
        library.sync_checkpoints().expect("checkpoint")[0].committed_through,
        0
    );

    let completed = reconcile_collection_heads_with_limits(
        &client,
        &mut library,
        wiki_id,
        collection_id,
        1_776_945_700,
        ReconciliationLimits {
            max_batches_per_page: 1,
            max_revisions_per_page: 1,
        },
    )
    .await
    .expect("next run resumes from the durable middle revision");
    assert!(!completed.resumed);
    assert_eq!(completed.revisions_captured, 1);
    assert_eq!(completed.status.state, SyncRunState::Succeeded);
    assert_eq!(
        library.sync_checkpoints().expect("checkpoint")[0].committed_through,
        1_776_945_700
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 8);
    assert!(requests[3].contains("rvstartid=1300000001"));
    assert!(requests[6].contains("rvstartid=1300000002"));
}
