mod support;

use std::path::Path;
use std::process::{Command, Output};
use std::thread;

use serde_json::Value;
use support::{FixtureResponse, FixtureServer};
use wikisync_core::{
    CollectionBudget, CollectionRemovalPolicy, CollectionRule, HistoryPolicy, InclusionReason,
    PageId, PageTitle, TitleSelection,
};
use wikisync_store::{
    CollectionPreviewCommit, DumpImportRequest, Library, ResolvedCollectionMember, SyncRunKind,
};
use wikisyncd::{ApplicationHandler, Client, Daemon};

const INDEX_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn current_dump_preview_is_complete_stable_json_and_performs_no_network_transfer() {
    let temporary = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(temporary.path()).expect("initialize library");
    let wiki_id = library
        .register_wiki("http://127.0.0.1:9/w/api.php", "en")
        .expect("register offline source");
    let title = PageTitle::new("Preview only").expect("title");
    let rule = CollectionRule::ExplicitTitles(
        TitleSelection::new([title.clone()]).expect("title selection"),
    );
    let member = ResolvedCollectionMember {
        page_id: PageId::new(42).expect("page ID"),
        namespace: 0,
        title: title.clone(),
        inclusion_reason: InclusionReason::ExplicitTitle(title),
    };
    let budget = CollectionBudget::unlimited()
        .with_maximum_pages(10)
        .expect("page budget")
        .with_maximum_bytes(1_000_000)
        .expect("byte budget");
    let (collection_id, _) = library
        .create_collection_from_preview(
            wiki_id,
            "Dump preview",
            CollectionPreviewCommit {
                rule: &rule,
                history_policy: HistoryPolicy::CurrentAndFuture,
                budget,
                removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                members: std::slice::from_ref(&member),
                missing_titles: &[],
                predicted_canonical_bytes: Some(100),
            },
        )
        .expect("create fixture collection");
    drop(library);

    // Port 9 has no fixture listener. A successful preview proves the default path
    // neither downloads the index nor asks MediaWiki to resolve anything.
    let output = run(
        temporary.path(),
        &[
            "dump-bootstrap",
            "--collection",
            &collection_id.get().to_string(),
            "--index-url",
            "http://127.0.0.1:9/enwiki/index.json",
            "--index-blake3",
            INDEX_DIGEST,
            "--expected-database",
            "enwiki",
            "--json",
        ],
    );
    assert_success(&output);
    let preview: Value = serde_json::from_slice(&output.stdout).expect("preview JSON");
    assert_eq!(preview["schema_version"], 1);
    assert_eq!(preview["operation"], "dump-bootstrap");
    assert_eq!(preview["committed"], false);
    assert_eq!(preview["source"]["wiki_id"], wiki_id.get());
    assert_eq!(preview["source"]["expected_database"], "enwiki");
    assert_eq!(preview["trust"]["index_blake3"], INDEX_DIGEST);
    assert_eq!(preview["trust"]["caller_retained_independently"], true);
    assert!(
        preview["trust"]["warning"]
            .as_str()
            .expect("trust warning")
            .contains("not an independent trust anchor")
    );
    assert_eq!(preview["collection_scope"]["resolved_page_count"], 1);
    assert_eq!(
        preview["ceilings"]["storage"]["hard_collection_maximum_pages"],
        10
    );
    assert_eq!(
        preview["ceilings"]["storage"]["hard_collection_maximum_canonical_bytes"],
        1_000_000
    );
    assert!(
        preview["ceilings"]["transfer"]["max_total_artifact_bytes"]
            .as_u64()
            .expect("transfer ceiling")
            > 0
    );
    assert!(
        preview["ceilings"]["parser"]["max_decompressed_bytes"]
            .as_u64()
            .expect("parser ceiling")
            > 0
    );

    let library = Library::open_read_only(temporary.path()).expect("read-only library");
    assert!(library.sync_run_statuses(20).expect("runs").is_empty());
}

#[test]
fn current_dump_parser_requires_an_explicit_complete_trust_identity() {
    let temporary = tempfile::tempdir().expect("temporary library");
    Library::open(temporary.path()).expect("initialize library");

    let incomplete = run(
        temporary.path(),
        &["dump-bootstrap", "--collection", "1", "--json"],
    );
    assert!(!incomplete.status.success());
    assert!(
        String::from_utf8_lossy(&incomplete.stderr).contains("requires --index-url"),
        "{}",
        String::from_utf8_lossy(&incomplete.stderr)
    );

    let weak_digest = run(
        temporary.path(),
        &[
            "dump-bootstrap",
            "--collection",
            "1",
            "--index-url",
            "https://dumps.wikimedia.org/enwiki/index.json",
            "--index-blake3",
            "1234",
            "--expected-database",
            "enwiki",
        ],
    );
    assert!(!weak_digest.status.success());
    assert!(String::from_utf8_lossy(&weak_digest.stderr).contains("64 hexadecimal digits"));
}

#[test]
fn status_surfaces_durable_dump_identity_cursor_bytes_state_and_failure() {
    let temporary = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(temporary.path()).expect("initialize library");
    let wiki_id = library
        .register_wiki("http://127.0.0.1:9/w/api.php", "en")
        .expect("register source");
    let title = PageTitle::new("Status fixture").expect("title");
    let rule =
        CollectionRule::ExplicitTitles(TitleSelection::new([title]).expect("title selection"));
    let collection_id = library
        .create_collection(
            wiki_id,
            "Dump status",
            &rule,
            HistoryPolicy::CurrentAndFuture,
            CollectionBudget::unlimited(),
            CollectionRemovalPolicy::StopTrackingRetainHistory,
        )
        .expect("create collection");
    let generation = library
        .collection_configuration(collection_id)
        .expect("configuration query")
        .expect("configuration")
        .generation;
    let started = library
        .start_or_resume_sync_run(
            wiki_id,
            Some(collection_id),
            SyncRunKind::Bootstrap,
            1_700_000_000,
        )
        .expect("start bootstrap run");
    let imported = library
        .claim_or_resume_dump_import(DumpImportRequest {
            run_id: started.status.run_id,
            dump_digest: &format!("b3:{INDEX_DIGEST}"),
            dump_compressed_bytes: 99_000,
            collection_generation: generation,
            bootstrap_started_at: started.status.checkpoint_candidate,
        })
        .expect("claim dump import");
    library
        .record_dump_import_progress(imported.status.import_id, 321)
        .expect("record scan cursor");
    library
        .fail_dump_import(
            imported.status.import_id,
            "fixture-interrupted",
            "offline fixture interruption",
            true,
        )
        .expect("record retained failure");
    drop(library);

    let json_output = run(temporary.path(), &["status", "--json"]);
    assert_success(&json_output);
    let status: Value = serde_json::from_slice(&json_output.stdout).expect("status JSON");
    let dump = &status["dump_imports"][0];
    assert_eq!(dump["run_id"], started.status.run_id);
    assert_eq!(
        dump["authenticated_index_digest"],
        format!("b3:{INDEX_DIGEST}")
    );
    assert_eq!(dump["dump_compressed_bytes"], 99_000);
    assert_eq!(dump["state"], "failed");
    assert_eq!(dump["progress"]["pages_scanned"], 321);
    assert_eq!(dump["progress"]["imported_pages"], 0);
    assert_eq!(dump["progress"]["imported_canonical_bytes"], 0);
    assert_eq!(dump["retryable"], true);
    assert_eq!(dump["latest_error"]["code"], "fixture-interrupted");

    let human = run(temporary.path(), &["status"]);
    assert_success(&human);
    let human = String::from_utf8(human.stdout).expect("human status UTF-8");
    assert!(
        human.contains(&format!("index b3:{INDEX_DIGEST}")),
        "{human}"
    );
    assert!(human.contains("cursor 321 pages"), "{human}");
    assert!(
        human.contains("authenticated dump set 99000 compressed bytes"),
        "{human}"
    );
    assert!(
        human.contains("fixture-interrupted; retryable=true"),
        "{human}"
    );
}

#[test]
fn dump_commit_forwards_to_the_daemon_writer_before_authenticated_acquisition_fails() {
    assert_dump_commit_reaches_fixture(true);
}

#[test]
fn dump_commit_uses_the_short_direct_writer_when_no_daemon_owns_the_library() {
    assert_dump_commit_reaches_fixture(false);
}

fn assert_dump_commit_reaches_fixture(with_daemon: bool) {
    let temporary = tempfile::tempdir().expect("temporary library");
    let server = FixtureServer::start(vec![FixtureResponse::json("{}")]);
    let mut library = Library::open(temporary.path()).expect("initialize library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("register fixture source");
    let title = PageTitle::new("Forwarded dump page").expect("title");
    let rule = CollectionRule::ExplicitTitles(
        TitleSelection::new([title.clone()]).expect("title selection"),
    );
    let member = ResolvedCollectionMember {
        page_id: PageId::new(77).expect("page ID"),
        namespace: 0,
        title: title.clone(),
        inclusion_reason: InclusionReason::ExplicitTitle(title),
    };
    let (collection_id, _) = library
        .create_collection_from_preview(
            wiki_id,
            "Forwarded dump",
            CollectionPreviewCommit {
                rule: &rule,
                history_policy: HistoryPolicy::CurrentAndFuture,
                budget: CollectionBudget::unlimited(),
                removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                members: std::slice::from_ref(&member),
                missing_titles: &[],
                predicted_canonical_bytes: None,
            },
        )
        .expect("create collection");
    drop(library);

    let daemon_thread = if with_daemon {
        let handler = ApplicationHandler::new(temporary.path()).expect("application handler");
        let daemon = Daemon::bind(temporary.path(), handler).expect("bind daemon");
        Some(thread::spawn(move || daemon.run()))
    } else {
        None
    };
    let index_url = server.endpoint().replace("/w/api.php", "/index.json");
    let output = run(
        temporary.path(),
        &[
            "dump-bootstrap",
            "--collection",
            &collection_id.get().to_string(),
            "--index-url",
            &index_url,
            "--index-blake3",
            INDEX_DIGEST,
            "--expected-database",
            "enwiki",
            "--commit",
            "--json",
        ],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("did not match its trusted BLAKE3 digest"),
        "{error}"
    );
    assert!(!error.contains("library is busy"), "{error}");

    if let Some(daemon_thread) = daemon_thread {
        Client::for_library(temporary.path())
            .expect("daemon client")
            .shutdown()
            .expect("shutdown daemon");
        daemon_thread
            .join()
            .expect("join daemon")
            .expect("daemon result");
    }
    let (_, requests) = server.finish();
    assert_eq!(
        requests.len(),
        1,
        "commit must download the index exactly once"
    );
    assert!(requests[0].starts_with("GET /index.json "));
}

fn run(library: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .arg("--library")
        .arg(library)
        .args(arguments)
        .output()
        .expect("run wikisync")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
