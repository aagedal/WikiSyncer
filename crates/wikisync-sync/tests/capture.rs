mod support;

use support::{FixtureResponse, FixtureServer};
use wikisync_core::{PageId, PageTitle, TitleSelection};
use wikisync_mediawiki::{ClientConfig, MediaWikiClient};
use wikisync_search::{SearchIndex, SearchQuery, SqliteSearchIndex};
use wikisync_store::Library;
use wikisync_sync::capture_explicit_titles;

const TITLE_RESOLUTION: &str = include_str!("../../../fixtures/mediawiki/title-resolution.json");
const REVISION_CONTENT: &str = include_str!("../../../fixtures/mediawiki/revision-content.json");

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
