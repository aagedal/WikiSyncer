mod support;

use std::process::Command;
use std::thread;

use serde_json::Value;
use support::{FixtureResponse, FixtureServer};
use wikisync_core::{CollectionId, HistoryPolicy};
use wikisync_store::{Library, ObjectKind};
use wikisync_sync::CollectionSelectionPreview;
use wikisyncd::{ApplicationHandler, Client, CollectionAdministration, CollectionDraft, Daemon};

const TITLE_RESOLUTION: &str = include_str!("../../../fixtures/mediawiki/title-resolution.json");

#[test]
fn verify_forwards_to_the_daemon_that_owns_the_library_writer() {
    let temporary = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(temporary.path()).expect("initialize library");
    library
        .put_bytes(ObjectKind::Wikitext, b"canonical fixture")
        .expect("store fixture object");
    drop(library);

    let handler = ApplicationHandler::new(temporary.path()).expect("application handler");
    let daemon = Daemon::bind(temporary.path(), handler).expect("bind daemon");
    let daemon_thread = thread::spawn(move || daemon.run());

    let output = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "--library",
            temporary.path().to_str().expect("UTF-8 fixture path"),
            "verify",
            "--full",
        ])
        .output()
        .expect("run CLI");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 output");
    assert!(stdout.contains("verification-complete"));
    assert!(stdout.contains("scope=full"));
    assert!(stdout.contains("fully_verified=true"));

    Client::for_library(temporary.path())
        .expect("daemon client")
        .shutdown()
        .expect("shutdown daemon");
    daemon_thread
        .join()
        .expect("join daemon")
        .expect("daemon result");
}

#[test]
fn collection_commit_forwards_the_complete_preview_to_the_daemon_writer() {
    let temporary = tempfile::tempdir().expect("temporary library");
    let server = FixtureServer::start(vec![FixtureResponse::json(TITLE_RESOLUTION)]);
    let mut library = Library::open(temporary.path()).expect("initialize library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("register fixture wiki");
    drop(library);

    let handler = ApplicationHandler::new(temporary.path()).expect("application handler");
    let daemon = Daemon::bind(temporary.path(), handler).expect("bind daemon");
    let daemon_thread = thread::spawn(move || daemon.run());

    let output = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "--library",
            temporary.path().to_str().expect("UTF-8 fixture path"),
            "collection",
            "add",
            "--wiki",
            &wiki_id.get().to_string(),
            "--name",
            "Forwarded",
            "--title",
            "Rust_programming_language",
            "--title",
            "Definitely missing WikiSyncer fixture page",
            "--commit",
            "--json",
        ])
        .output()
        .expect("run collection add");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).expect("collection receipt JSON");
    assert_eq!(receipt["committed"], true);
    assert_eq!(receipt["result"]["kind"], "added");
    assert_eq!(receipt["preview"]["resolved_page_count"], 1);
    assert_eq!(receipt["preview"]["missing_title_count"], 1);

    Client::for_library(temporary.path())
        .expect("daemon client")
        .shutdown()
        .expect("shutdown daemon");
    daemon_thread
        .join()
        .expect("join daemon")
        .expect("daemon result");
    let library = Library::open_read_only(temporary.path()).expect("read library");
    let collection = library.collections().expect("collections");
    assert_eq!(collection.len(), 1);
    assert_eq!(collection[0].name, "Forwarded");
    assert_eq!(
        library
            .unresolved_titles(collection[0].collection_id)
            .expect("unresolved titles")
            .len(),
        1
    );

    let (_, requests) = server.finish();
    assert_eq!(requests.len(), 1);
}

#[test]
fn stale_forwarded_edit_is_rejected_without_overwriting_daemon_changes() {
    let temporary = tempfile::tempdir().expect("temporary library");
    let library_root = temporary.path().to_path_buf();
    let hook_root = library_root.clone();
    let server = FixtureServer::start_with_hook(
        vec![
            FixtureResponse::json(TITLE_RESOLUTION),
            FixtureResponse::json(TITLE_RESOLUTION),
        ],
        move |index| {
            if index != 1 {
                return;
            }
            let library = Library::open_read_only(&hook_root).expect("read concurrent draft");
            let collection_id = CollectionId::new(1).expect("collection");
            let configuration = library
                .collection_configuration(collection_id)
                .expect("configuration query")
                .expect("configuration");
            let estimate = library
                .collection_estimate(collection_id)
                .expect("estimate");
            let preview = CollectionSelectionPreview {
                rule: configuration.rule.clone(),
                members: library
                    .resolved_collection_members(collection_id)
                    .expect("members"),
                missing_titles: library
                    .unresolved_titles(collection_id)
                    .expect("unresolved titles"),
                predicted_canonical_bytes: estimate.predicted_canonical_bytes,
                category_batches: 0,
            };
            let draft = CollectionDraft {
                wiki_id: configuration.wiki_id,
                name: "Daemon concurrent rename".to_owned(),
                preview,
                history_policy: configuration.history_policy,
                budget: configuration.budget,
                removal_policy: configuration.removal_policy,
            };
            drop(library);
            Client::for_library(&hook_root)
                .expect("daemon client")
                .administer_collection(CollectionAdministration::Edit {
                    collection_id,
                    expected_generation: configuration.generation,
                    draft,
                })
                .expect("concurrent daemon edit");
        },
    );
    let mut library = Library::open(&library_root).expect("initialize library");
    let wiki_id = library
        .register_wiki(server.endpoint(), "en")
        .expect("register fixture wiki");
    drop(library);
    let add = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "--library",
            library_root.to_str().expect("UTF-8 fixture path"),
            "collection",
            "add",
            "--wiki",
            &wiki_id.get().to_string(),
            "--name",
            "Original",
            "--title",
            "Rust_programming_language",
            "--title",
            "Definitely missing WikiSyncer fixture page",
            "--commit",
        ])
        .output()
        .expect("add original collection");
    assert!(
        add.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let handler = ApplicationHandler::new(&library_root).expect("application handler");
    let daemon = Daemon::bind(&library_root, handler).expect("bind daemon");
    let daemon_thread = thread::spawn(move || daemon.run());
    let stale = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "--library",
            library_root.to_str().expect("UTF-8 fixture path"),
            "collection",
            "edit",
            "--collection",
            "1",
            "--name",
            "Stale forwarded replacement",
            "--title",
            "Rust_programming_language",
            "--title",
            "Definitely missing WikiSyncer fixture page",
            "--history",
            "complete",
            "--commit",
        ])
        .output()
        .expect("run stale forwarded edit");
    assert!(!stale.status.success());
    let error = String::from_utf8_lossy(&stale.stderr);
    assert!(error.contains("no changes were committed"), "{error}");
    assert!(error.contains("re-run collection edit"), "{error}");
    assert!(error.contains("re-preview"), "{error}");

    Client::for_library(&library_root)
        .expect("daemon client")
        .shutdown()
        .expect("shutdown daemon");
    daemon_thread
        .join()
        .expect("join daemon")
        .expect("daemon result");
    let library = Library::open_read_only(&library_root).expect("read library");
    let configuration = library
        .collection_configuration(CollectionId::new(1).expect("collection"))
        .expect("configuration query")
        .expect("configuration");
    assert_eq!(configuration.name, "Daemon concurrent rename");
    assert_eq!(configuration.generation, 2);
    assert_eq!(
        configuration.history_policy,
        HistoryPolicy::CurrentAndFuture
    );
    assert_eq!(
        library
            .resolved_collection_members(configuration.collection_id)
            .expect("members")
            .len(),
        1
    );
    assert_eq!(
        library
            .unresolved_titles(configuration.collection_id)
            .expect("unresolved")
            .len(),
        1
    );
    drop(library);
    let (_, requests) = server.finish();
    assert_eq!(requests.len(), 2);
}

#[test]
fn source_add_and_unused_remove_forward_to_the_daemon_writer() {
    let temporary = tempfile::tempdir().expect("temporary library");
    Library::open(temporary.path()).expect("initialize library");
    let handler = ApplicationHandler::new(temporary.path()).expect("application handler");
    let daemon = Daemon::bind(temporary.path(), handler).expect("bind daemon");
    let daemon_thread = thread::spawn(move || daemon.run());

    let added = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "--library",
            temporary.path().to_str().expect("UTF-8 fixture path"),
            "source",
            "add",
            "--api-endpoint",
            "https://fixture.example/w/api.php",
            "--language",
            "fixture",
            "--json",
        ])
        .output()
        .expect("forward source add");
    assert!(
        added.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&added.stdout),
        String::from_utf8_lossy(&added.stderr),
    );
    let added_json: Value = serde_json::from_slice(&added.stdout).expect("source add JSON");
    assert_eq!(added_json["wiki_id"], 1);
    assert_eq!(
        added_json["api_endpoint"],
        "https://fixture.example/w/api.php"
    );
    assert_eq!(added_json["language_code"], "fixture");
    assert_eq!(
        Library::open_read_only(temporary.path())
            .expect("read added source")
            .wikis()
            .expect("sources")
            .len(),
        1
    );

    let removed = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "--library",
            temporary.path().to_str().expect("UTF-8 fixture path"),
            "source",
            "remove",
            "--wiki",
            "1",
            "--json",
        ])
        .output()
        .expect("forward source remove");
    assert!(
        removed.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&removed.stdout),
        String::from_utf8_lossy(&removed.stderr),
    );
    let removed_json: Value = serde_json::from_slice(&removed.stdout).expect("source remove JSON");
    assert_eq!(removed_json["wiki_id"], 1);
    assert_eq!(removed_json["removed"], true);
    assert!(
        Library::open_read_only(temporary.path())
            .expect("read removed source")
            .wikis()
            .expect("sources")
            .is_empty()
    );

    Client::for_library(temporary.path())
        .expect("daemon client")
        .shutdown()
        .expect("shutdown daemon");
    daemon_thread
        .join()
        .expect("join daemon")
        .expect("daemon result");
}

#[test]
fn forwarded_source_remove_preserves_an_in_use_source() {
    let temporary = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(temporary.path()).expect("initialize library");
    let wiki_id = library
        .register_wiki("https://used.example/w/api.php", "used")
        .expect("register source");
    library
        .create_explicit_collection(wiki_id, "Uses source")
        .expect("create collection");
    drop(library);
    let handler = ApplicationHandler::new(temporary.path()).expect("application handler");
    let daemon = Daemon::bind(temporary.path(), handler).expect("bind daemon");
    let daemon_thread = thread::spawn(move || daemon.run());

    let removal = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "--library",
            temporary.path().to_str().expect("UTF-8 fixture path"),
            "source",
            "remove",
            "--wiki",
            &wiki_id.get().to_string(),
            "--json",
        ])
        .output()
        .expect("forward in-use source remove");
    assert!(!removal.status.success());
    let error = String::from_utf8_lossy(&removal.stderr);
    assert!(error.contains("still in use"), "{error}");
    assert!(error.contains("1 collections"), "{error}");
    assert_eq!(
        Library::open_read_only(temporary.path())
            .expect("read preserved source")
            .wikis()
            .expect("sources")
            .len(),
        1
    );

    Client::for_library(temporary.path())
        .expect("daemon client")
        .shutdown()
        .expect("shutdown daemon");
    daemon_thread
        .join()
        .expect("join daemon")
        .expect("daemon result");
}
