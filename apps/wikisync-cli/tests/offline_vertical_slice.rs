mod support;

use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Output};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use support::{FixtureResponse, FixtureServer};
use tower::ServiceExt;
use wikisync_core::{PageTitle, TitleSelection};
use wikisync_mediawiki::{ClientConfig, MediaWikiClient};
use wikisync_store::Library;
use wikisync_sync::{capture_explicit_titles, capture_revision_history};

const TITLE_RESOLUTION: &str = include_str!("../../../fixtures/mediawiki/title-resolution.json");
const REVISION_CONTENT: &str = include_str!("../../../fixtures/mediawiki/revision-content.json");
const REVISIONS_PAGE_1: &str = include_str!("../../../fixtures/mediawiki/revisions-page-1.json");
const REVISIONS_PAGE_2: &str = include_str!("../../../fixtures/mediawiki/revisions-page-2.json");
const REVISION_CONTENT_OLDER: &str =
    include_str!("../../../fixtures/mediawiki/revision-content-older.json");

#[tokio::test(flavor = "multi_thread")]
async fn captured_library_remains_readable_searchable_and_diffable_offline() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(REVISION_CONTENT),
        FixtureResponse::json(REVISIONS_PAGE_1),
        FixtureResponse::json(REVISIONS_PAGE_2),
        FixtureResponse::json(REVISION_CONTENT_OLDER),
    ]);
    let directory = tempfile::tempdir().expect("temporary library");
    let library_root = directory.path().to_path_buf();
    let client = MediaWikiClient::new(
        ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 offline-slice-test")
            .expect("client configuration"),
    )
    .expect("client");
    let mut library = Library::open(&library_root).expect("library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Offline fixture")
        .expect("collection");
    let selection =
        TitleSelection::new([PageTitle::new("Rust programming language").expect("title")])
            .expect("selection");

    let capture =
        capture_explicit_titles(&client, &mut library, wiki_id, collection_id, &selection)
            .await
            .expect("current revision capture");
    assert_eq!(capture.pages.len(), 1);
    let page = &capture.pages[0];
    let history = capture_revision_history(&client, &mut library, wiki_id, page.page_id)
        .await
        .expect("history capture");
    assert_eq!(history.revisions_captured, 1);
    drop(library);
    drop(client);

    let (fixture_address, requests) = server.finish();
    assert_eq!(requests.len(), 5);
    assert!(
        TcpStream::connect(fixture_address).is_err(),
        "fixture source must be unavailable before offline assertions"
    );

    let search = run_json(&library_root, &["search", "--json", "systems programming"]);
    assert_eq!(search[0]["title"], "Rust (programming language)");
    assert_eq!(search[0]["revision_id"], 1_300_000_001_u64);

    let article = run_json(
        &library_root,
        &[
            "show",
            "--wiki",
            "1",
            "--json",
            "Rust (programming language)",
        ],
    );
    assert_eq!(article["format"], "markdown");
    assert!(
        article["content"]
            .as_str()
            .unwrap()
            .contains("systems programming")
    );

    let history = run_json(
        &library_root,
        &[
            "history",
            "--wiki",
            "1",
            "--json",
            "Rust (programming language)",
        ],
    );
    assert_eq!(history["revisions"].as_array().unwrap().len(), 2);

    let older = run_json(
        &library_root,
        &[
            "show",
            "--wiki",
            "1",
            "--revision",
            "1300000000",
            "--source",
            "--json",
            "Rust (programming language)",
        ],
    );
    assert_eq!(older["format"], "wikitext");
    assert_eq!(older["content"], "== Rust ==\nA programming language.");

    let difference = run_json(
        &library_root,
        &[
            "diff",
            "--wiki",
            "1",
            "--reading",
            "--json",
            "1300000000",
            "1300000001",
        ],
    );
    assert_eq!(difference["mode"], "reading");
    assert_eq!(difference["has_changes"], true);

    let response = wikisync_web::router(&library_root)
        .oneshot(
            Request::builder()
                .uri("/search?q=systems")
                .body(Body::empty())
                .expect("reader request"),
        )
        .await
        .expect("reader response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("reader body");
    let body = String::from_utf8(body.to_vec()).expect("UTF-8 reader body");
    assert!(body.contains("Rust (programming language)"));
    assert!(body.contains("A systems programming language."));
}

fn run_json(library_root: &Path, arguments: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .arg("--library")
        .arg(library_root)
        .args(arguments)
        .output()
        .expect("run wikisync command");
    assert_success(&output, arguments);
    serde_json::from_slice(&output.stdout).expect("command JSON output")
}

fn assert_success(output: &Output, arguments: &[&str]) {
    assert!(
        output.status.success(),
        "wikisync {arguments:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
