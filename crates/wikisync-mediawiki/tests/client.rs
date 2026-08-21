mod support;

use std::time::Duration;

use support::{FixtureResponse, FixtureServer};
use wikisync_core::{PageId, PageTitle};
use wikisync_mediawiki::{
    CategoryMemberKind, ClientConfig, ClientError, MediaWikiClient, RevisionOrder, TitleResolution,
};

const TITLE_RESOLUTION: &str = include_str!("../../../fixtures/mediawiki/title-resolution.json");
const REVISIONS_PAGE_1: &str = include_str!("../../../fixtures/mediawiki/revisions-page-1.json");
const REVISIONS_PAGE_2: &str = include_str!("../../../fixtures/mediawiki/revisions-page-2.json");
const REVISION_CONTENT: &str = include_str!("../../../fixtures/mediawiki/revision-content.json");
const MAXLAG: &str = include_str!("../../../fixtures/mediawiki/maxlag.json");
const EMPTY_PAGES: &str = include_str!("../../../fixtures/mediawiki/empty-pages.json");
const CATEGORY_MEMBERS_PAGE_1: &str =
    include_str!("../../../fixtures/mediawiki/category-members-page-1.json");
const CATEGORY_MEMBERS_PAGE_2: &str =
    include_str!("../../../fixtures/mediawiki/category-members-page-2.json");

fn fixture_client(server: &FixtureServer) -> MediaWikiClient {
    let config = ClientConfig::new(
        server.endpoint(),
        "WikiSyncer/0.1 fixture-tests (https://github.com/aagedal/WikiSyncer)",
    )
    .expect("valid fixture config");
    MediaWikiClient::new(config).expect("fixture client")
}

#[tokio::test(flavor = "multi_thread")]
async fn resolves_titles_with_required_safety_parameters() {
    let server = FixtureServer::start(vec![FixtureResponse::json(TITLE_RESOLUTION)]);
    let client = fixture_client(&server);
    let titles = [
        PageTitle::new("Rust_programming_language").expect("valid title"),
        PageTitle::new("Definitely missing WikiSyncer fixture page").expect("valid title"),
    ];

    let results = client
        .resolve_titles(&titles)
        .await
        .expect("resolve titles");
    assert_eq!(results.len(), 2);
    let TitleResolution::Found(found) = &results[0] else {
        panic!("first title should resolve");
    };
    assert_eq!(found.page_id.get(), 25_357_340);
    assert_eq!(found.title.as_str(), "Rust (programming language)");
    assert_eq!(
        found
            .current_revision
            .as_ref()
            .expect("head revision")
            .revision_id
            .get(),
        1_300_000_001
    );
    assert!(matches!(results[1], TitleResolution::Missing { .. }));

    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request.starts_with("GET /w/api.php?"));
    assert!(request.contains("action=query"));
    assert!(request.contains("formatversion=2"));
    assert!(request.contains("maxlag=5"));
    assert!(request.contains("redirects=1"));
    assert!(request.contains("titles=Rust_programming_language%7CDefinitely+missing"));
    assert!(request.contains("user-agent: WikiSyncer/0.1 fixture-tests"));
}

#[tokio::test(flavor = "multi_thread")]
async fn one_hundred_title_spike_is_split_into_bounded_requests() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(EMPTY_PAGES),
        FixtureResponse::json(EMPTY_PAGES),
    ]);
    let client = fixture_client(&server);
    let titles = (0..100)
        .map(|index| PageTitle::new(format!("Fixture page {index}")))
        .collect::<Result<Vec<_>, _>>()
        .expect("valid fixture titles");

    let results = client
        .resolve_titles(&titles)
        .await
        .expect("resolve title batches");
    assert!(results.is_empty());

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("Fixture+page+0"));
    assert!(requests[0].contains("Fixture+page+49"));
    assert!(!requests[0].contains("Fixture+page+50"));
    assert!(requests[1].contains("Fixture+page+50"));
    assert!(requests[1].contains("Fixture+page+99"));
}

#[tokio::test(flavor = "multi_thread")]
async fn revision_continuation_round_trips_as_opaque_values() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(REVISIONS_PAGE_1),
        FixtureResponse::json(REVISIONS_PAGE_2),
    ]);
    let client = fixture_client(&server);
    let page_id = PageId::new(25_357_340).expect("page ID");

    let first = client
        .revision_batch(page_id, RevisionOrder::NewestFirst, None)
        .await
        .expect("first revision page");
    assert_eq!(first.revisions.len(), 1);
    assert!(first.revisions[0].minor);
    assert_eq!(
        first.revisions[0].content_model.as_deref(),
        Some("wikitext")
    );
    let continuation = first.continuation.expect("continuation token");

    let second = client
        .revision_batch(page_id, RevisionOrder::NewestFirst, Some(&continuation))
        .await
        .expect("second revision page");
    assert_eq!(second.revisions.len(), 1);
    assert_eq!(second.revisions[0].revision_id.get(), 1_300_000_000);
    assert_eq!(second.revisions[0].parent_id, None);
    assert_eq!(second.revisions[0].user, None);
    assert_eq!(second.revisions[0].sha1, None);
    assert_eq!(second.continuation, None);

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].contains("rvcontinue="));
    assert!(requests[1].contains("continue=%7C%7C"));
    assert!(requests[1].contains("rvcontinue=20260818100000%7C1299999999"));
    assert!(requests[1].contains("rvdir=older"));
    assert!(requests[1].contains("rvlimit=500"));
}

#[tokio::test(flavor = "multi_thread")]
async fn category_members_are_namespace_filtered_and_continuation_round_trips() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(CATEGORY_MEMBERS_PAGE_1),
        FixtureResponse::json(CATEGORY_MEMBERS_PAGE_2),
    ]);
    let client = fixture_client(&server);
    let category = PageTitle::new("Category:Root").expect("category title");

    let first = client
        .category_members_batch(&category, None)
        .await
        .expect("first category page");
    assert_eq!(first.members.len(), 2);
    assert_eq!(first.members[0].kind, CategoryMemberKind::Page);
    assert_eq!(first.members[1].kind, CategoryMemberKind::Subcategory);
    let continuation = first.continuation.expect("continuation");

    let second = client
        .category_members_batch(&category, Some(&continuation))
        .await
        .expect("second category page");
    assert_eq!(second.members.len(), 2);
    assert!(second.continuation.is_none());

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("list=categorymembers"));
    assert!(requests[0].contains("cmtitle=Category%3ARoot"));
    assert!(requests[0].contains("cmtype=page%7Csubcat"));
    assert!(requests[0].contains("cmnamespace=0%7C14"));
    assert!(requests[0].contains("cmlimit=500"));
    assert!(!requests[0].contains("cmcontinue="));
    assert!(requests[1].contains("continue=-%7C%7C"));
    assert!(requests[1].contains("cmcontinue=page%7C42455441%7C0%7CBETA"));
}

#[tokio::test(flavor = "multi_thread")]
async fn fetches_one_exact_revision_content_with_metadata() {
    let server = FixtureServer::start(vec![FixtureResponse::json(REVISION_CONTENT)]);
    let client = fixture_client(&server);
    let page_id = PageId::new(25_357_340).expect("page ID");
    let revision_id = 1_300_000_001_u64.try_into().expect("revision ID");

    let revision = client
        .revision_content(page_id, revision_id)
        .await
        .expect("revision content");
    assert_eq!(revision.metadata.revision_id, revision_id);
    assert_eq!(revision.metadata.content_model.as_deref(), Some("wikitext"));
    assert_eq!(
        revision.source,
        b"== Rust ==\nA systems programming language."
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("pageids=25357340"));
    assert!(requests[0].contains("rvstartid=1300000001"));
    assert!(requests[0].contains("rvendid=1300000001"));
    assert!(requests[0].contains("contentmodel%7Ccontent"));
}

#[tokio::test(flavor = "multi_thread")]
async fn maxlag_errors_preserve_retry_guidance() {
    let server = FixtureServer::start(vec![FixtureResponse {
        status: 200,
        body: MAXLAG,
        retry_after: Some(7),
    }]);
    let client = fixture_client(&server);
    let error = client
        .resolve_titles(&[PageTitle::new("Rust").expect("valid title")])
        .await
        .expect_err("maxlag should fail");

    assert!(error.is_retryable());
    assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
    assert!(matches!(
        error,
        ClientError::Api(ref api) if api.code == "maxlag"
    ));
    server.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn declared_oversized_responses_are_rejected_before_parsing() {
    let server = FixtureServer::start(vec![FixtureResponse::json(TITLE_RESOLUTION)]);
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 tests")
        .expect("valid config")
        .with_max_response_bytes(32)
        .expect("positive response limit");
    let client = MediaWikiClient::new(config).expect("fixture client");

    let error = client
        .resolve_titles(&[PageTitle::new("Rust").expect("valid title")])
        .await
        .expect_err("fixture exceeds limit");
    assert!(matches!(error, ClientError::ResponseTooLarge { limit: 32 }));
    server.finish();
}
