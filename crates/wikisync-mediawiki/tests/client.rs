#[allow(dead_code)]
mod support;

use std::time::{Duration, Instant};

use support::{FixtureResponse, FixtureServer};
use wikisync_core::{PageId, PageTitle};
use wikisync_mediawiki::{
    CategoryMemberKind, ClientConfig, ClientError, MediaWikiClient, RecentChangeKind,
    RecentChangesContinuation, RetryPolicy, RevisionOrder, TitleResolution,
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
const RECENT_CHANGES_PAGE_1: &str = include_str!("fixtures/recent-changes-page-1.json");
const RECENT_CHANGES_PAGE_2: &str = include_str!("fixtures/recent-changes-page-2.json");
const SOURCE_TIMESTAMP: &str = r#"{"batchcomplete":true,"curtimestamp":"2026-08-30T12:05:00Z"}"#;
const RECENT_CHANGES_WRONG_NAMESPACE: &str = r#"{
  "curtimestamp":"2026-08-30T12:06:00Z",
  "query":{"recentchanges":[{
    "type":"edit","ns":1,"title":"Talk:Wrong","pageid":1,
    "revid":2,"old_revid":1,"rcid":3,"timestamp":"2026-08-30T12:05:00Z"
  }]}
}"#;

fn fixture_client(server: &FixtureServer) -> MediaWikiClient {
    let config = ClientConfig::new(
        server.endpoint(),
        "WikiSyncer/0.1 fixture-tests (https://github.com/aagedal/WikiSyncer)",
    )
    .expect("valid fixture config");
    MediaWikiClient::new(config).expect("fixture client")
}

fn fast_retry_policy(maximum_attempts: usize) -> RetryPolicy {
    RetryPolicy::new(
        maximum_attempts,
        Duration::from_millis(1),
        Duration::from_millis(20),
    )
    .expect("fast retry policy")
    .with_circuit_breaker(3, Duration::from_millis(100))
    .expect("fast circuit breaker")
}

fn fixture_client_with_policy(server: &FixtureServer, policy: RetryPolicy) -> MediaWikiClient {
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 retry-tests")
        .expect("valid fixture config")
        .with_retry_policy(policy);
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
async fn recent_changes_stream_forward_with_identifiers_logs_and_opaque_continuation() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(SOURCE_TIMESTAMP),
        FixtureResponse::json(RECENT_CHANGES_PAGE_1),
        FixtureResponse::json(RECENT_CHANGES_PAGE_2),
    ]);
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 RecentChanges tests")
        .expect("fixture config")
        .with_recent_changes_per_request(3)
        .expect("bounded RecentChanges page size");
    let client = MediaWikiClient::new(config).expect("fixture client");
    let race_window_end = client.source_timestamp().await.expect("source timestamp");
    assert_eq!(race_window_end, "2026-08-30T12:05:00Z");

    let first = client
        .recent_changes_batch("2026-08-30T12:00:00Z", Some(&race_window_end), None)
        .await
        .expect("first RecentChanges page");
    assert_eq!(first.source_timestamp, "2026-08-30T12:05:00Z");
    assert_eq!(first.changes.len(), 2, "unrelated protect log is skipped");
    assert_eq!(first.changes[0].change_id, 7001);
    assert_eq!(first.changes[0].kind, RecentChangeKind::Edit);
    assert_eq!(first.changes[0].page_id.expect("page ID").get(), 101);
    assert_eq!(
        first.changes[0].revision_id.expect("revision ID").get(),
        1002
    );
    assert_eq!(
        first.changes[0].old_revision_id.expect("old ID").get(),
        1001
    );
    assert!(first.changes[0].minor);
    assert_eq!(first.changes[1].kind, RecentChangeKind::New);
    assert_eq!(first.changes[1].old_revision_id, None);
    assert_eq!(
        first.changes[1].user_id, None,
        "anonymous user ID is absent"
    );
    let continuation = first.continuation.expect("continuation");
    assert_eq!(continuation.generic(), "-||");
    assert_eq!(continuation.recent_changes(), "20260830120200|7003");
    let continuation = RecentChangesContinuation::from_parts(
        continuation.generic(),
        continuation.recent_changes(),
    )
    .expect("persisted continuation reconstructs");

    let second = client
        .recent_changes_batch(
            "2026-08-30T12:00:00Z",
            Some("2026-08-30T12:05:00Z"),
            Some(&continuation),
        )
        .await
        .expect("second RecentChanges page");
    assert_eq!(second.source_timestamp, "2026-08-30T12:06:00Z");
    assert!(second.continuation.is_none());
    assert_eq!(
        second
            .changes
            .iter()
            .map(|change| change.kind)
            .collect::<Vec<_>>(),
        vec![
            RecentChangeKind::Move,
            RecentChangeKind::Delete,
            RecentChangeKind::Restore
        ]
    );
    let move_log = second.changes[0].log.as_ref().expect("move log metadata");
    assert_eq!(move_log.log_id, 8001);
    assert_eq!(move_log.target_namespace, Some(0));
    assert_eq!(
        move_log
            .target_title
            .as_ref()
            .expect("move target")
            .as_str(),
        "New title"
    );
    assert_eq!(second.changes[1].page_id, None);
    assert_eq!(
        second.changes[2]
            .log
            .as_ref()
            .expect("restore log")
            .parameters["count"]["revisions"],
        3
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains("action=query"));
    assert!(requests[0].contains("curtimestamp=1"));
    assert!(!requests[0].contains("list=recentchanges"));
    assert!(requests[1].contains("list=recentchanges"));
    assert!(requests[1].contains("rcdir=newer"));
    assert!(requests[1].contains("rcnamespace=0"));
    assert!(requests[1].contains("rctype=edit%7Cnew%7Clog"));
    assert!(requests[1].contains("rclimit=3"));
    assert!(requests[1].contains("curtimestamp=1"));
    assert!(requests[1].contains("rcstart=2026-08-30T12%3A00%3A00Z"));
    assert!(requests[1].contains("rcend=2026-08-30T12%3A05%3A00Z"));
    assert!(!requests[1].contains("rccontinue="));
    assert!(requests[2].contains("continue=-%7C%7C"));
    assert!(requests[2].contains("rccontinue=20260830120200%7C7003"));
}

#[tokio::test(flavor = "multi_thread")]
async fn recent_changes_reject_invalid_windows_and_namespace_amplification() {
    let no_request_server = FixtureServer::start(Vec::new());
    let client = fixture_client(&no_request_server);
    assert!(matches!(
        client
            .recent_changes_batch("2026-02-30T00:00:00Z", None, None)
            .await,
        Err(ClientError::InvalidRequest(_))
    ));
    assert!(matches!(
        client
            .recent_changes_batch("2026-08-30T12:00:00Z", Some("2026-08-30T11:59:59Z"), None)
            .await,
        Err(ClientError::InvalidRequest(_))
    ));
    assert!(no_request_server.finish().is_empty());

    let server = FixtureServer::start(vec![FixtureResponse::json(RECENT_CHANGES_WRONG_NAMESPACE)]);
    let client = fixture_client(&server);
    assert!(matches!(
        client
            .recent_changes_batch("2026-08-30T12:00:00Z", None, None)
            .await,
        Err(ClientError::InvalidResponse(
            "RecentChanges response contained a row outside namespace 0"
        ))
    ));
    server.finish();
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
        ..FixtureResponse::json(MAXLAG)
    }]);
    let client = fixture_client_with_policy(&server, fast_retry_policy(1));
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
async fn retryable_status_then_success_repeats_the_same_bounded_request() {
    let server = FixtureServer::start(vec![
        FixtureResponse {
            status: 503,
            body: "{}",
            retry_after: None,
            ..FixtureResponse::json("{}")
        },
        FixtureResponse::json(TITLE_RESOLUTION),
    ]);
    let client = fixture_client_with_policy(&server, fast_retry_policy(2));
    let results = client
        .resolve_titles(&[PageTitle::new("Rust").expect("title")])
        .await
        .expect("retry succeeds");
    assert!(matches!(results.first(), Some(TitleResolution::Found(_))));

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].lines().next(),
        requests[1].lines().next(),
        "the retry must repeat exactly the same API page"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn retryable_failures_stop_at_the_configured_attempt_ceiling() {
    let throttled = FixtureResponse {
        status: 503,
        body: "{}",
        retry_after: None,
        ..FixtureResponse::json("{}")
    };
    let server = FixtureServer::start(vec![throttled.clone(), throttled.clone(), throttled]);
    let client = fixture_client_with_policy(&server, fast_retry_policy(3));
    let error = client
        .resolve_titles(&[PageTitle::new("Rust").expect("title")])
        .await
        .expect_err("retry budget must be exhausted");
    assert!(matches!(
        error,
        ClientError::HttpStatus { status, .. }
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    ));
    assert_eq!(server.finish().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_after_is_honored_up_to_the_configured_safety_ceiling() {
    let server = FixtureServer::start(vec![
        FixtureResponse {
            status: 429,
            body: "{}",
            retry_after: Some(1),
            ..FixtureResponse::json("{}")
        },
        FixtureResponse::json(TITLE_RESOLUTION),
    ]);
    let client = fixture_client_with_policy(&server, fast_retry_policy(2));
    let started = Instant::now();
    client
        .resolve_titles(&[PageTitle::new("Rust").expect("title")])
        .await
        .expect("retry after throttle");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(15),
        "server delay should be clamped to and honor the 20 ms safety ceiling: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(1));
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn nonretryable_status_is_attempted_only_once() {
    let server = FixtureServer::start(vec![FixtureResponse {
        status: 400,
        body: "{}",
        retry_after: None,
        ..FixtureResponse::json("{}")
    }]);
    let client = fixture_client_with_policy(&server, fast_retry_policy(4));
    let error = client
        .resolve_titles(&[PageTitle::new("Rust").expect("title")])
        .await
        .expect_err("bad request is not retryable");
    assert!(!error.is_retryable());
    assert!(matches!(
        error,
        ClientError::HttpStatus { status, .. } if status == reqwest::StatusCode::BAD_REQUEST
    ));
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn cloned_clients_share_an_open_circuit_without_an_extra_request() {
    let server = FixtureServer::start(vec![FixtureResponse {
        status: 503,
        body: "{}",
        retry_after: None,
        ..FixtureResponse::json("{}")
    }]);
    let policy = fast_retry_policy(1)
        .with_circuit_breaker(1, Duration::from_secs(1))
        .expect("single-failure circuit");
    let client = fixture_client_with_policy(&server, policy);
    let clone = client.clone();
    client
        .resolve_titles(&[PageTitle::new("Rust").expect("title")])
        .await
        .expect_err("first request opens the circuit");
    let error = clone
        .resolve_titles(&[PageTitle::new("Ferris").expect("title")])
        .await
        .expect_err("shared circuit rejects the clone without networking");
    assert!(matches!(error, ClientError::CircuitOpen { .. }));
    assert!(error.is_retryable());
    assert!(error.retry_after().is_some_and(|delay| !delay.is_zero()));
    assert_eq!(server.finish().len(), 1);
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

#[tokio::test(flavor = "multi_thread")]
async fn cross_origin_redirect_is_rejected_without_contacting_the_destination() {
    let server = FixtureServer::start(vec![FixtureResponse::redirect(
        "http://localhost:9/w/api.php".to_owned(),
    )]);
    let client = fixture_client(&server);

    let error = client
        .resolve_titles(&[PageTitle::new("Rust").expect("valid title")])
        .await
        .expect_err("cross-origin redirect must fail closed");

    assert!(matches!(
        error,
        ClientError::Transport(ref transport) if transport.is_redirect()
    ));
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn same_origin_redirect_remains_within_the_source_allowlist() {
    let server = FixtureServer::start(vec![
        FixtureResponse::redirect("/w/redirected-api.php".to_owned()),
        FixtureResponse::json(TITLE_RESOLUTION),
    ]);
    let client = fixture_client(&server);

    let resolved = client
        .resolve_titles(&[PageTitle::new("Rust").expect("valid title")])
        .await
        .expect("same-origin redirect is allowed");

    assert!(matches!(resolved.first(), Some(TitleResolution::Found(_))));
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].starts_with("GET /w/redirected-api.php "));
}

#[tokio::test(flavor = "multi_thread")]
async fn cloned_clients_share_one_aggregate_download_budget() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(TITLE_RESOLUTION),
    ]);
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 budget-tests")
        .expect("valid config")
        .with_max_downloaded_response_bytes_per_run(TITLE_RESOLUTION.len())
        .expect("positive aggregate limit")
        .with_max_downloaded_response_bytes_per_second(Some(usize::MAX))
        .expect("positive byte rate");
    let client = MediaWikiClient::new(config).expect("fixture client");
    let clone = client.clone();

    client
        .resolve_titles(&[PageTitle::new("Rust").expect("valid title")])
        .await
        .expect("first response fits the run budget");
    let error = clone
        .resolve_titles(&[PageTitle::new("Ferris").expect("valid title")])
        .await
        .expect_err("clone must not receive a fresh byte budget");

    assert!(matches!(
        error,
        ClientError::DownloadBudgetExceeded {
            limit,
            downloaded,
            ..
        } if limit == TITLE_RESOLUTION.len() && downloaded == TITLE_RESOLUTION.len()
    ));
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn cloned_clients_share_one_bounded_download_rate() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(EMPTY_PAGES),
        FixtureResponse::json(EMPTY_PAGES),
    ]);
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 rate-tests")
        .expect("valid config")
        .with_max_downloaded_response_bytes_per_second(Some(100))
        .expect("positive byte rate");
    let client = MediaWikiClient::new(config).expect("fixture client");
    let clone = client.clone();
    let started = Instant::now();

    let first = tokio::spawn(async move {
        client
            .resolve_titles(&[PageTitle::new("First").expect("valid title")])
            .await
    });
    let second = tokio::spawn(async move {
        clone
            .resolve_titles(&[PageTitle::new("Second").expect("valid title")])
            .await
    });
    first
        .await
        .expect("first task did not panic")
        .expect("first response succeeds");
    second
        .await
        .expect("second task did not panic")
        .expect("second response succeeds");

    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(950),
        "the two {}-byte bodies must share the configured rate; elapsed {elapsed:?}",
        EMPTY_PAGES.len()
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "rate shaping took unexpectedly long: {elapsed:?}"
    );
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_response_bodies_also_consume_the_download_rate() {
    let server = FixtureServer::start(vec![
        FixtureResponse {
            status: 503,
            body: TITLE_RESOLUTION,
            retry_after: None,
            location: None,
            delay: Duration::ZERO,
        },
        FixtureResponse::json(TITLE_RESOLUTION),
    ]);
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 retry-rate-tests")
        .expect("valid config")
        .with_max_downloaded_response_bytes_per_second(Some(1_000))
        .expect("positive byte rate")
        .with_retry_policy(fast_retry_policy(2));
    let client = MediaWikiClient::new(config).expect("fixture client");
    let started = Instant::now();

    client
        .resolve_titles(&[PageTitle::new("Rust").expect("valid title")])
        .await
        .expect("retry succeeds");

    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1_250),
        "both {}-byte attempt bodies must be shaped; elapsed {elapsed:?}",
        TITLE_RESOLUTION.len()
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "retry rate shaping took unexpectedly long: {elapsed:?}"
    );
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn cloned_clients_enforce_the_shared_concurrent_request_limit() {
    let responses = (0..4)
        .map(|_| FixtureResponse::delayed_json(EMPTY_PAGES, Duration::from_millis(75)))
        .collect();
    let server = FixtureServer::start(responses);
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 concurrency-tests")
        .expect("valid config")
        .with_max_concurrent_requests(2)
        .expect("positive concurrency limit");
    let client = MediaWikiClient::new(config).expect("fixture client");
    let mut tasks = Vec::new();
    for index in 0..4 {
        let clone = client.clone();
        tasks.push(tokio::spawn(async move {
            let title = PageTitle::new(format!("Fixture {index}")).expect("valid title");
            clone.resolve_titles(&[title]).await
        }));
    }
    for task in tasks {
        task.await
            .expect("request task did not panic")
            .expect("fixture request succeeds");
    }

    assert_eq!(server.maximum_concurrent_requests(), 2);
    assert_eq!(server.finish().len(), 4);
}
