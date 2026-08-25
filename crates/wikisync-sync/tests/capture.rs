mod support;

use bzip2::Compression;
use bzip2::write::BzEncoder;
use std::io::Write;
use std::time::Duration;
use support::{FixtureResponse, FixtureServer};
use wikisync_core::{
    CollectionBudget, CollectionRemovalPolicy, CollectionRule, HistoryPolicy, ImagePolicy,
    InclusionReason, PageId, PageTitle, RevisionId, ThumbnailPolicy, TitleSelection,
};
use wikisync_mediawiki::{
    ClientConfig, DumpAcquisitionLimits, DumpDigest, DumpLimits, MediaWikiClient, RetryPolicy,
    TrustedDumpIndex,
};
use wikisync_search::{SearchIndex, SearchQuery, SqliteSearchIndex};
use wikisync_store::{
    CollectionPreviewCommit, DumpImportState, Library, ObjectId, ObjectKind,
    ResolvedCollectionMember, SyncRunKind, SyncRunState,
};
use wikisync_sync::{
    CaptureError, CategoryPreviewError, CategoryPreviewLimits, CollectionPreviewError,
    DumpBootstrapError, DynamicMembershipReconciliation, DynamicMembershipReconciliationError,
    ReconciliationLimits, bootstrap_collection_from_verified_dump, capture_committed_collection,
    capture_explicit_titles, capture_revision_history, commit_collection_preview, parse_title_list,
    preview_category_selection, preview_collection_rule, reconcile_collection_heads,
    reconcile_collection_heads_with_cancellation, reconcile_collection_heads_with_limits,
    reconcile_dynamic_collection_membership,
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
const CATEGORY_MEMBERS_RUST: &str =
    include_str!("../../../fixtures/mediawiki/category-members-rust.json");
const CATEGORY_MEMBERS_RUST_AND_ALPHA: &str = r#"
{
  "batchcomplete": true,
  "query": {
    "categorymembers": [
      {"pageid": 25357340, "ns": 0, "title": "Rust (programming language)"},
      {"pageid": 101, "ns": 0, "title": "Alpha"}
    ]
  }
}
"#;
const CATEGORY_MEMBERS_ALPHA: &str = r#"
{
  "batchcomplete": true,
  "query": {
    "categorymembers": [
      {"pageid": 101, "ns": 0, "title": "Alpha"}
    ]
  }
}
"#;
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
const REVISION_IMAGES: &str = r#"
{"parse":{"pageid":25357340,"revid":1300000001,"images":["Fixture.png"]}}
"#;
const THUMBNAIL_METADATA: &str = r#"
{
  "query": {
    "pages": [{
      "pageid": 9001,
      "ns": 6,
      "title": "File:Fixture.png",
      "imageinfo": [{
        "sha1": "abcdef0123456789abcdef0123456789",
        "mime": "image/png",
        "thumburl": "{{ENDPOINT}}?fixture-thumbnail=1",
        "thumbwidth": 1,
        "thumbheight": 1,
        "descriptionurl": "{{ENDPOINT}}?fixture-description=1",
        "extmetadata": {
          "Artist": {"value": "Fixture photographer"},
          "Credit": {"value": "Fixture photographer / fixture source"},
          "LicenseShortName": {"value": "CC BY-SA 4.0"},
          "LicenseUrl": {"value": "https://creativecommons.org/licenses/by-sa/4.0/"}
        }
      }]
    }]
  }
}
"#;
const VALID_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_dump_bootstrap_filters_selection_and_closes_its_race_window() {
    let server = FixtureServer::start_generated(|endpoint| {
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>Fixture Wikipedia</sitename><dbname>enwiki</dbname>
    <base>{endpoint}</base><generator>MediaWiki fixture</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter" /></namespaces>
  </siteinfo>
  <page><title>Alpha</title><ns>0</ns><id>10</id><revision>
    <id>100</id><parentid>99</parentid><timestamp>2026-08-23T10:00:00Z</timestamp>
    <contributor><username>Fixture editor</username><id>42</id></contributor>
    <comment>dump head</comment><model>wikitext</model><format>text/x-wiki</format>
    <text bytes="5" xml:space="preserve">Alpha</text>
  </revision></page>
  <page><title>Not selected</title><ns>0</ns><id>20</id><revision>
    <id>200</id><timestamp>2026-08-23T10:01:00Z</timestamp>
    <contributor><username>Fixture editor</username></contributor>
    <model>wikitext</model><format>text/x-wiki</format>
    <text bytes="7" xml:space="preserve">Ignored</text>
  </revision></page>
</mediawiki>"#
        );
        let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(xml.as_bytes()).expect("compress XML");
        let artifact = encoder.finish().expect("finish bzip2 member");
        let artifact_digest = blake3::hash(&artifact).to_hex().to_string();
        let index = format!(
            r#"{{"schema":"wikisync-current-dump-index-v1","database":"enwiki","generated_at":"2026-08-23T10:02:00Z","artifacts":[{{"kind":"pages-meta-current-multistream","path":"fixture-current.xml.bz2","bytes":{},"blake3":"{artifact_digest}"}}]}}"#,
            artifact.len()
        );
        let unchanged = r#"{
          "batchcomplete":true,"query":{"pages":[{
            "pageid":10,"ns":0,"title":"Alpha","revisions":[{
              "revid":100,"parentid":99,"timestamp":"2026-08-23T10:00:00Z","size":5
            }]
          }]}}
        "#;
        let missing = r#"{
          "batchcomplete":true,"query":{"pages":[{
            "pageid":-1,"ns":0,"title":"","missing":true
          }]}}
        "#;
        vec![
            FixtureResponse::json(index),
            FixtureResponse::bytes(artifact, "application/x-bzip2"),
            FixtureResponse::json(unchanged),
            FixtureResponse::status_json(503, "{}"),
            FixtureResponse::json(missing),
            FixtureResponse::json(unchanged),
            FixtureResponse::json(missing),
        ]
    });
    let retry_policy = RetryPolicy::new(1, Duration::from_millis(1), Duration::from_millis(1))
        .expect("single-attempt retry policy");
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 dump-bootstrap-test")
            .expect("client configuration")
            .with_retry_policy(retry_policy),
    )
    .expect("client");
    let cache = tempfile::tempdir().expect("dump cache");
    let index_body = {
        // Recreate only the authenticated index bytes to derive the out-of-band trust
        // anchor. The artifact response itself remains available solely via acquisition.
        let endpoint = server.endpoint();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>Fixture Wikipedia</sitename><dbname>enwiki</dbname>
    <base>{endpoint}</base><generator>MediaWiki fixture</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter" /></namespaces>
  </siteinfo>
  <page><title>Alpha</title><ns>0</ns><id>10</id><revision>
    <id>100</id><parentid>99</parentid><timestamp>2026-08-23T10:00:00Z</timestamp>
    <contributor><username>Fixture editor</username><id>42</id></contributor>
    <comment>dump head</comment><model>wikitext</model><format>text/x-wiki</format>
    <text bytes="5" xml:space="preserve">Alpha</text>
  </revision></page>
  <page><title>Not selected</title><ns>0</ns><id>20</id><revision>
    <id>200</id><timestamp>2026-08-23T10:01:00Z</timestamp>
    <contributor><username>Fixture editor</username></contributor>
    <model>wikitext</model><format>text/x-wiki</format>
    <text bytes="7" xml:space="preserve">Ignored</text>
  </revision></page>
</mediawiki>"#
        );
        let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(xml.as_bytes()).expect("compress XML");
        let artifact = encoder.finish().expect("finish bzip2 member");
        format!(
            r#"{{"schema":"wikisync-current-dump-index-v1","database":"enwiki","generated_at":"2026-08-23T10:02:00Z","artifacts":[{{"kind":"pages-meta-current-multistream","path":"fixture-current.xml.bz2","bytes":{},"blake3":"{}"}}]}}"#,
            artifact.len(),
            blake3::hash(&artifact).to_hex()
        )
    };
    let trust = TrustedDumpIndex::new(
        server.endpoint(),
        DumpDigest::from_hex(blake3::hash(index_body.as_bytes()).to_hex().as_ref())
            .expect("index digest"),
        "enwiki",
    )
    .expect("trust anchor");
    let dump_set = client
        .acquire_current_dump_set(&trust, cache.path(), DumpAcquisitionLimits::default())
        .await
        .expect("authenticated dump acquisition");

    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let alpha = PageTitle::new("Alpha").unwrap();
    let gone = PageTitle::new("Gone").unwrap();
    let selection = TitleSelection::new([alpha.clone(), gone.clone()]).unwrap();
    let rule = CollectionRule::ExplicitTitles(selection);
    let members = [
        ResolvedCollectionMember {
            page_id: PageId::new(10).unwrap(),
            namespace: 0,
            title: alpha.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(alpha.clone()),
        },
        ResolvedCollectionMember {
            page_id: PageId::new(30).unwrap(),
            namespace: 0,
            title: gone.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(gone),
        },
    ];
    let budget = CollectionBudget::unlimited()
        .with_maximum_pages(2)
        .unwrap()
        .with_maximum_bytes(5)
        .unwrap();
    let (collection_id, _) = library
        .create_collection_from_preview(
            wiki_id,
            "Dump selection",
            CollectionPreviewCommit {
                rule: &rule,
                history_policy: HistoryPolicy::CurrentAndFuture,
                budget,
                removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                members: &members,
                missing_titles: &[],
                predicted_canonical_bytes: Some(5),
            },
        )
        .expect("collection");

    let other_server = FixtureServer::start(vec![]);
    let other_client = MediaWikiClient::new(
        ClientConfig::new(
            other_server.endpoint(),
            "WikiSyncer/0.1 wrong-dump-closure-source-test",
        )
        .expect("other client configuration"),
    )
    .expect("other client");
    let error = bootstrap_collection_from_verified_dump(
        &other_client,
        &mut library,
        collection_id,
        &dump_set,
        DumpLimits::default(),
    )
    .await
    .expect_err("closure client must match the durable wiki source");
    assert!(matches!(error, DumpBootstrapError::ClientEndpointMismatch));
    assert!(library.sync_run_statuses(10).unwrap().is_empty());
    other_server.finish();

    let error = bootstrap_collection_from_verified_dump(
        &client,
        &mut library,
        collection_id,
        &dump_set,
        DumpLimits::default(),
    )
    .await
    .expect_err("retryable closure failure");
    assert!(matches!(
        error,
        DumpBootstrapError::Capture(CaptureError::Source(_))
    ));
    let interrupted = library.sync_run_statuses(1).unwrap().remove(0);
    assert_eq!(interrupted.state, SyncRunState::Running);
    assert_eq!(interrupted.succeeded_jobs, 1);
    assert_eq!(interrupted.failed_jobs, 1);
    let import = library
        .dump_import_status(interrupted.run_id)
        .unwrap()
        .unwrap();
    assert_eq!(import.state, DumpImportState::Failed);
    assert!(import.retryable);
    assert_eq!(import.pages_scanned, 2);
    assert_eq!(import.imported_pages, 1);
    assert_eq!(import.imported_canonical_bytes, 5);
    assert_eq!(
        library
            .sync_checkpoints()
            .unwrap()
            .into_iter()
            .find(|checkpoint| checkpoint.collection_id == Some(collection_id))
            .unwrap()
            .committed_through,
        0
    );

    let resumed = bootstrap_collection_from_verified_dump(
        &client,
        &mut library,
        collection_id,
        &dump_set,
        DumpLimits::default(),
    )
    .await
    .expect("resumed dump bootstrap");
    assert!(resumed.resumed);
    assert_eq!(resumed.status.run_id, interrupted.run_id);
    assert_eq!(resumed.pages_imported, 0);
    assert_eq!(resumed.pages_reused, 0);
    assert_eq!(resumed.pages_absent_from_dump, 1);
    assert_eq!(resumed.closure.pages_checked, 1);
    assert_eq!(resumed.closure.missing_pages, 1);
    assert_eq!(resumed.import.state, DumpImportState::Succeeded);
    assert_eq!(resumed.import.pages_scanned, 2);
    assert_eq!(
        library
            .revision(wiki_id, RevisionId::new(100).unwrap())
            .unwrap()
            .unwrap()
            .content_object_id,
        ObjectId::for_bytes(ObjectKind::Wikitext, b"Alpha")
    );
    assert!(
        library
            .page(wiki_id, PageId::new(20).unwrap())
            .unwrap()
            .is_none()
    );

    let idempotent = bootstrap_collection_from_verified_dump(
        &client,
        &mut library,
        collection_id,
        &dump_set,
        DumpLimits::default(),
    )
    .await
    .expect("idempotent dump bootstrap");
    assert_eq!(idempotent.pages_imported, 0);
    assert_eq!(idempotent.pages_reused, 1);
    assert_eq!(library.logical_object_count().unwrap(), 1);
    let checkpoint = library
        .sync_checkpoints()
        .unwrap()
        .into_iter()
        .find(|checkpoint| checkpoint.collection_id == Some(collection_id))
        .unwrap();
    assert_eq!(checkpoint.last_run_id, Some(idempotent.status.run_id));
    assert!(checkpoint.committed_through >= resumed.status.checkpoint_candidate);
    assert_eq!(server.finish().len(), 7);
}

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
    assert_eq!(
        preview
            .pages
            .iter()
            .map(|page| (page.title.as_str(), page.category_depth))
            .collect::<Vec<_>>(),
        [("Alpha", 0), ("Beta", 0), ("Gamma", 1)]
    );
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
async fn thumbnail_policy_captures_validated_attributed_media_after_text() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(REVISION_IMAGES),
        FixtureResponse::json(THUMBNAIL_METADATA),
        FixtureResponse::bytes(VALID_PNG.to_vec(), "image/png"),
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(REVISION_IMAGES),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 thumbnail-sync-test")
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
    library
        .set_collection_image_policy(
            collection_id,
            ImagePolicy::Thumbnails(ThumbnailPolicy::new(64, 2, 1024).expect("thumbnail policy")),
        )
        .expect("enable thumbnails");
    let selection =
        TitleSelection::new([PageTitle::new("Rust_programming_language").expect("title")])
            .expect("selection");

    let first = capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
        .await
        .expect("capture with media");
    assert_eq!(first.media.placements_discovered, 1);
    assert_eq!(first.media.placements_captured, 1);
    assert!(first.media.failures.is_empty());
    let media = library
        .revision_media(wiki_id, first.pages[0].revision_id)
        .expect("revision media");
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].author, "Fixture photographer");
    assert_eq!(
        media[0].attribution,
        "Fixture photographer / fixture source"
    );
    assert_eq!(media[0].license_name, "CC BY-SA 4.0");
    assert_eq!(
        library
            .read_object(media[0].content_object_id)
            .expect("thumbnail object"),
        VALID_PNG
    );

    let second = capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
        .await
        .expect("idempotent media capture");
    assert_eq!(second.media.placements_discovered, 1);
    assert_eq!(second.media.placements_reused, 1);
    assert_eq!(second.media.placements_captured, 0);
    assert!(second.media.failures.is_empty());

    let requests = server.finish();
    assert_eq!(requests.len(), 8);
    assert!(requests[2].contains("action=parse"));
    assert!(requests[3].contains("prop=imageinfo"));
    assert!(requests[4].contains("fixture-thumbnail=1"));
    assert!(requests[7].contains("action=parse"));
}

#[tokio::test(flavor = "multi_thread")]
async fn title_list_preview_commit_and_capture_are_explicit_and_durable() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(RECONCILIATION_UNCHANGED_TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 title-list-test")
            .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let selection = parse_title_list(
        "Rust_programming_language\nDefinitely missing WikiSyncer fixture page\n",
        10_000,
    )
    .expect("title list");
    let rule = CollectionRule::TitleList(selection);
    let preview = preview_collection_rule(&client, &rule, CategoryPreviewLimits::default())
        .await
        .expect("preview");
    assert_eq!(preview.members.len(), 1);
    assert_eq!(preview.missing_titles.len(), 1);
    assert!(library.collections().unwrap().is_empty());

    let collection_id = library
        .create_collection(
            wiki_id,
            "Imported pages",
            &rule,
            HistoryPolicy::CurrentAndFuture,
            CollectionBudget::unlimited(),
            CollectionRemovalPolicy::StopTrackingRetainHistory,
        )
        .expect("collection");
    let committed = commit_collection_preview(
        &mut library,
        collection_id,
        &preview,
        HistoryPolicy::CurrentAndFuture,
        CollectionBudget::unlimited(),
        CollectionRemovalPolicy::StopTrackingRetainHistory,
    )
    .expect("commit preview");
    assert_eq!(committed.active_members, 1);
    assert!(matches!(
        library.resolved_collection_members(collection_id).unwrap()[0].inclusion_reason,
        InclusionReason::TitleList(_)
    ));

    let captured = capture_committed_collection(&client, &mut library, collection_id)
        .await
        .expect("capture committed collection");
    assert_eq!(captured.pages.len(), 1);
    assert!(
        library
            .page(wiki_id, captured.pages[0].page_id)
            .unwrap()
            .is_some()
    );
    drop(library);
    let reopened = Library::open(directory.path()).expect("reopen");
    assert_eq!(
        reopened
            .resolved_collection_members(collection_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .collection_pages(wiki_id, collection_id)
            .unwrap()
            .len(),
        1
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn category_preview_commit_captures_members_with_category_reason() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(CATEGORY_MEMBERS_RUST),
        FixtureResponse::json(RECONCILIATION_UNCHANGED_TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 category-commit-test")
            .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let rule = CollectionRule::Category {
        title: PageTitle::new("Category:Systems programming languages").unwrap(),
        recursion_depth: 0,
    };
    let preview = preview_collection_rule(&client, &rule, CategoryPreviewLimits::default())
        .await
        .expect("category preview");
    let collection_id = library
        .create_collection(
            wiki_id,
            "Systems languages",
            &rule,
            HistoryPolicy::CurrentAndFuture,
            CollectionBudget::unlimited(),
            CollectionRemovalPolicy::StopTrackingRetainHistory,
        )
        .expect("collection");
    commit_collection_preview(
        &mut library,
        collection_id,
        &preview,
        HistoryPolicy::CurrentAndFuture,
        CollectionBudget::unlimited(),
        CollectionRemovalPolicy::StopTrackingRetainHistory,
    )
    .expect("commit");
    assert!(matches!(
        library.resolved_collection_members(collection_id).unwrap()[0].inclusion_reason,
        InclusionReason::Category { depth: 0, .. }
    ));
    let captured = capture_committed_collection(&client, &mut library, collection_id)
        .await
        .expect("capture category member");
    assert_eq!(captured.pages.len(), 1);
    assert_eq!(
        library
            .collection_pages(wiki_id, collection_id)
            .unwrap()
            .len(),
        1
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_category_reconciliation_adds_members_and_removes_tracking_without_erasing_history()
{
    let server = FixtureServer::start(vec![
        FixtureResponse::json(CATEGORY_MEMBERS_RUST),
        FixtureResponse::json(RECONCILIATION_UNCHANGED_TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(CATEGORY_MEMBERS_RUST_AND_ALPHA),
        FixtureResponse::json(CATEGORY_MEMBERS_ALPHA),
    ]);
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 dynamic-category-test")
            .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let rule = CollectionRule::Category {
        title: PageTitle::new("Category:Systems programming languages").unwrap(),
        recursion_depth: 0,
    };
    let preview = preview_collection_rule(&client, &rule, CategoryPreviewLimits::default())
        .await
        .expect("initial category preview");
    let collection_id = library
        .create_collection(
            wiki_id,
            "Dynamic systems languages",
            &rule,
            HistoryPolicy::CurrentAndFuture,
            CollectionBudget::unlimited(),
            CollectionRemovalPolicy::StopTrackingRetainHistory,
        )
        .expect("collection");
    commit_collection_preview(
        &mut library,
        collection_id,
        &preview,
        HistoryPolicy::CurrentAndFuture,
        CollectionBudget::unlimited(),
        CollectionRemovalPolicy::StopTrackingRetainHistory,
    )
    .expect("initial membership");
    let captured = capture_committed_collection(&client, &mut library, collection_id)
        .await
        .expect("capture initial member");
    let rust_page_id = captured.pages[0].page_id;
    let rust_revision_id = captured.pages[0].revision_id;
    let rust_object_id = captured.pages[0].content_object_id;

    let added = reconcile_dynamic_collection_membership(
        &client,
        &mut library,
        collection_id,
        CategoryPreviewLimits::default(),
    )
    .await
    .expect("add newly resolved member");
    assert_eq!(
        added,
        DynamicMembershipReconciliation::Category {
            category_batches: 1,
            membership: wikisync_store::MembershipCommit {
                active_members: 2,
                removed_members: 0,
            },
        }
    );
    assert_eq!(
        library
            .resolved_collection_members(collection_id)
            .unwrap()
            .iter()
            .map(|member| member.page_id.get())
            .collect::<Vec<_>>(),
        [101, rust_page_id.get()]
    );

    let removed = reconcile_dynamic_collection_membership(
        &client,
        &mut library,
        collection_id,
        CategoryPreviewLimits::default(),
    )
    .await
    .expect("remove no-longer-resolved member");
    assert_eq!(
        removed,
        DynamicMembershipReconciliation::Category {
            category_batches: 1,
            membership: wikisync_store::MembershipCommit {
                active_members: 1,
                removed_members: 1,
            },
        }
    );
    assert_eq!(
        library
            .resolved_collection_members(collection_id)
            .unwrap()
            .iter()
            .map(|member| member.page_id.get())
            .collect::<Vec<_>>(),
        [101]
    );
    assert!(
        library
            .collection_pages(wiki_id, collection_id)
            .unwrap()
            .is_empty(),
        "the removed page is no longer synchronized and the added page is not captured yet"
    );
    assert!(library.page(wiki_id, rust_page_id).unwrap().is_some());
    let retained_revision = library
        .revision(wiki_id, rust_revision_id)
        .unwrap()
        .expect("captured revision remains available");
    assert_eq!(retained_revision.content_object_id, rust_object_id);
    assert_eq!(
        library.read_object(rust_object_id).unwrap(),
        b"== Rust ==\nA systems programming language."
    );

    assert_eq!(server.finish().len(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_membership_reconciliation_is_a_network_free_no_op_for_static_rules() {
    let server = FixtureServer::start(vec![]);
    let endpoint = server.endpoint().to_owned();
    let client = MediaWikiClient::new(
        ClientConfig::new(&endpoint, "WikiSyncer/0.1 static-membership-test")
            .expect("client configuration"),
    )
    .expect("client");
    assert!(server.finish().is_empty());

    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library.register_wiki(&endpoint, "en").expect("wiki");
    let selection = TitleSelection::new([PageTitle::new("Rust").unwrap()]).unwrap();
    let collection_id = library
        .create_collection(
            wiki_id,
            "Static titles",
            &CollectionRule::ExplicitTitles(selection),
            HistoryPolicy::CurrentAndFuture,
            CollectionBudget::unlimited(),
            CollectionRemovalPolicy::StopTrackingRetainHistory,
        )
        .expect("collection");
    let configuration_before = library.collection_configuration(collection_id).unwrap();
    let members_before = library.resolved_collection_members(collection_id).unwrap();

    let result = reconcile_dynamic_collection_membership(
        &client,
        &mut library,
        collection_id,
        CategoryPreviewLimits::default(),
    )
    .await
    .expect("static no-op");
    assert_eq!(result, DynamicMembershipReconciliation::StaticRule);
    assert_eq!(
        library.collection_configuration(collection_id).unwrap(),
        configuration_before
    );
    assert_eq!(
        library.resolved_collection_members(collection_id).unwrap(),
        members_before
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_dynamic_category_previews_leave_membership_unchanged() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(CATEGORY_MEMBERS_RUST),
        FixtureResponse::json(CATEGORY_MEMBERS_RUST_AND_ALPHA),
        FixtureResponse::json(CATEGORY_MEMBERS_RUST_AND_ALPHA),
    ]);
    let endpoint = server.endpoint().to_owned();
    let client = MediaWikiClient::new(
        ClientConfig::new(&endpoint, "WikiSyncer/0.1 category-atomicity-test")
            .expect("client configuration"),
    )
    .expect("client");
    let directory = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(directory.path()).expect("library");
    let wiki_id = library.register_wiki(&endpoint, "en").expect("wiki");
    let rule = CollectionRule::Category {
        title: PageTitle::new("Category:Systems programming languages").unwrap(),
        recursion_depth: 0,
    };
    let budget = CollectionBudget::unlimited()
        .with_maximum_pages(1)
        .expect("page budget");
    let preview = preview_collection_rule(&client, &rule, CategoryPreviewLimits::default())
        .await
        .expect("initial category preview");
    let collection_id = library
        .create_collection(
            wiki_id,
            "Budgeted category",
            &rule,
            HistoryPolicy::CurrentAndFuture,
            budget,
            CollectionRemovalPolicy::StopTrackingRetainHistory,
        )
        .expect("collection");
    commit_collection_preview(
        &mut library,
        collection_id,
        &preview,
        HistoryPolicy::CurrentAndFuture,
        budget,
        CollectionRemovalPolicy::StopTrackingRetainHistory,
    )
    .expect("initial membership");
    let members_before = library.resolved_collection_members(collection_id).unwrap();

    let bounded = reconcile_dynamic_collection_membership(
        &client,
        &mut library,
        collection_id,
        CategoryPreviewLimits {
            max_pages: 1,
            ..CategoryPreviewLimits::default()
        },
    )
    .await
    .expect_err("bounded preview rejects oversized result");
    assert!(matches!(
        bounded,
        DynamicMembershipReconciliationError::Preview(CollectionPreviewError::Category(
            CategoryPreviewError::PageLimitExceeded { limit: 1 }
        ))
    ));
    assert_eq!(
        library.resolved_collection_members(collection_id).unwrap(),
        members_before
    );

    let oversized = reconcile_dynamic_collection_membership(
        &client,
        &mut library,
        collection_id,
        CategoryPreviewLimits::default(),
    )
    .await
    .expect_err("configured page budget rejects larger resolution");
    assert!(matches!(
        oversized,
        DynamicMembershipReconciliationError::Store(
            wikisync_store::StoreError::CollectionBudgetExceeded {
                resource: "pages",
                limit: 1,
                estimated: 2,
            }
        )
    ));
    assert_eq!(
        library.resolved_collection_members(collection_id).unwrap(),
        members_before
    );
    assert_eq!(server.finish().len(), 3);

    let network_failure = reconcile_dynamic_collection_membership(
        &client,
        &mut library,
        collection_id,
        CategoryPreviewLimits::default(),
    )
    .await
    .expect_err("closed fixture source fails preview");
    assert!(matches!(
        network_failure,
        DynamicMembershipReconciliationError::Preview(CollectionPreviewError::Category(
            CategoryPreviewError::Source(_)
        ))
    ));
    assert_eq!(
        library.resolved_collection_members(collection_id).unwrap(),
        members_before
    );
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
    let prior_run = library
        .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Update, 1)
        .expect("prior run");
    library
        .complete_sync_run(prior_run.status.run_id, None)
        .expect("complete prior run without its manifest");
    assert_eq!(library.manifest_count().expect("no manifest yet"), 0);

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
    assert_eq!(library.manifest_count().expect("manifest count"), 2);
    let repaired = library.read_manifest(1).expect("repaired predecessor");
    let repaired_sync = repaired.manifest.sync().expect("sync manifest event");
    assert_eq!(repaired_sync.run_id, prior_run.status.run_id);
    assert_eq!(repaired_sync.introduced_revisions.len(), 1);
    let manifest = library.read_manifest(2).expect("sync manifest");
    let manifest_sync = manifest.manifest.sync().expect("sync manifest event");
    assert_eq!(manifest_sync.run_id, report.status.run_id);
    assert_eq!(manifest_sync.wiki_id, wiki_id);
    assert_eq!(manifest_sync.collection_id, Some(collection_id));
    assert_eq!(manifest_sync.page_heads.len(), 1);
    assert_eq!(manifest_sync.page_heads[0].page_id, page_id);
    assert_eq!(
        manifest_sync.page_heads[0].revision_id,
        Some(RevisionId::new(1_300_000_003).expect("manifest head"))
    );
    assert_eq!(manifest_sync.introduced_revisions.len(), 2);
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
async fn cancelled_reconciliation_resumes_from_durable_content_before_advancing_head() {
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
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 cancellation-test")
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
    let middle_revision = RevisionId::new(1_300_000_002).expect("middle revision");

    let cancel_after_middle_is_durable = || {
        Library::open(directory.path())
            .expect("observe cancellation boundary")
            .revision(wiki_id, middle_revision)
            .expect("middle revision lookup")
            .is_some()
    };
    let error = reconcile_collection_heads_with_cancellation(
        &client,
        &mut library,
        wiki_id,
        collection_id,
        1_776_945_600,
        &cancel_after_middle_is_durable,
    )
    .await
    .expect_err("durable middle revision triggers cancellation");
    assert!(matches!(error, CaptureError::Cancelled));

    let middle = library
        .revision(wiki_id, middle_revision)
        .expect("middle revision lookup")
        .expect("middle revision remains durable");
    assert_eq!(
        library
            .page(wiki_id, initial.pages[0].page_id)
            .expect("page lookup")
            .expect("page")
            .current_revision_id,
        Some(original_head),
        "a cancelled gap must not expose its uncompleted remote head"
    );
    assert_eq!(
        library.sync_checkpoints().expect("checkpoint")[0].committed_through,
        0
    );
    let interrupted = library.sync_run_statuses(1).expect("run status").remove(0);
    assert_eq!(interrupted.state, SyncRunState::Running);
    assert_eq!(interrupted.running_jobs, 1);
    assert_eq!(interrupted.failed_jobs, 0);
    assert!(interrupted.latest_error.is_none());

    let completed = reconcile_collection_heads_with_cancellation(
        &client,
        &mut library,
        wiki_id,
        collection_id,
        1_776_945_700,
        &|| false,
    )
    .await
    .expect("resume cancelled reconciliation");
    assert!(completed.resumed);
    assert_eq!(completed.status.run_id, interrupted.run_id);
    assert_eq!(completed.status.state, SyncRunState::Succeeded);
    assert_eq!(completed.status.checkpoint_candidate, 1_776_945_600);
    assert_eq!(completed.revisions_captured, 1);
    assert_eq!(
        library
            .revision(wiki_id, middle_revision)
            .expect("middle revision lookup after resume")
            .expect("middle revision after resume")
            .content_object_id,
        middle.content_object_id,
        "resume must reuse the canonical object captured before cancellation"
    );
    assert_eq!(
        library
            .page(wiki_id, initial.pages[0].page_id)
            .expect("page lookup after resume")
            .expect("page after resume")
            .current_revision_id
            .expect("completed head")
            .get(),
        1_300_000_003
    );
    assert_eq!(
        library.sync_checkpoints().expect("completed checkpoint")[0].committed_through,
        1_776_945_600
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 8);
    assert!(requests[3].contains("rvstartid=1300000001"));
    assert!(requests[6].contains("rvstartid=1300000002"));
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
