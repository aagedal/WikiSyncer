mod support;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use support::{FixtureResponse, FixtureServer};
use wikisync_core::HistoryPolicy;
use wikisync_store::Library;
use wikisyncd::WriterAccess;

const TITLE_RESOLUTION: &str = include_str!("../../../fixtures/mediawiki/title-resolution.json");

#[test]
fn collection_lifecycle_previews_then_commits_with_stable_json() {
    let temporary = tempfile::tempdir().expect("temporary parent");
    let library = temporary.path().join("library");
    assert_success(&run(&library, &["init"]));

    let server = FixtureServer::start(vec![
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(TITLE_RESOLUTION),
        FixtureResponse::json(TITLE_RESOLUTION),
    ]);
    assert_success(&run(
        &library,
        &[
            "source",
            "add",
            "--api-endpoint",
            server.endpoint(),
            "--language",
            "en",
        ],
    ));

    let scope = [
        "collection",
        "add",
        "--wiki",
        "1",
        "--name",
        "Reference",
        "--title",
        "Rust_programming_language",
        "--title",
        "Definitely missing WikiSyncer fixture page",
        "--history",
        "last-n:5",
        "--max-pages",
        "10",
        "--json",
    ];
    let preview = run(&library, &scope);
    assert_success(&preview);
    let preview_json = parse_json(&preview);
    assert_eq!(preview_json["schema_version"], 1);
    assert_eq!(preview_json["operation"], "add");
    assert_eq!(preview_json["committed"], false);
    assert_eq!(preview_json["preview"]["complete"], true);
    assert_eq!(preview_json["preview"]["resolved_page_count"], 1);
    assert_eq!(preview_json["preview"]["missing_title_count"], 1);
    assert_eq!(preview_json["preview"]["predicted_canonical_bytes"], 42);
    assert_eq!(preview_json["preview"]["budget_assessment"], "fits");
    assert_eq!(preview_json["preview"]["members"][0]["page_id"], 25_357_340);
    assert!(
        parse_json(&run(&library, &["collection", "list", "--json"]))["collections"]
            .as_array()
            .expect("collections")
            .is_empty(),
        "preview must not mutate the library"
    );

    let mut commit_scope = scope.to_vec();
    commit_scope.insert(commit_scope.len() - 1, "--commit");
    let committed = run(&library, &commit_scope);
    assert_success(&committed);
    let committed_json = parse_json(&committed);
    assert_eq!(committed_json["committed"], true);
    assert_eq!(committed_json["result"]["kind"], "added");
    assert_eq!(committed_json["result"]["collection_id"], 1);

    let listed = parse_json(&run(&library, &["collection", "list", "--json"]));
    assert_eq!(listed["schema_version"], 1);
    assert_eq!(listed["includes_tombstones"], false);
    assert_eq!(listed["collections"][0]["status"], "active");
    assert_eq!(
        listed["collections"][0]["configuration"]["history_policy"]["kind"],
        "last-n"
    );
    assert_eq!(
        listed["collections"][0]["estimate"]["resolved_page_count"],
        1
    );

    let edited = run(
        &library,
        &[
            "collection",
            "edit",
            "--collection",
            "1",
            "--name",
            "Renamed",
            "--history",
            "complete",
            "--commit",
            "--json",
        ],
    );
    assert_success(&edited);
    let edited_json = parse_json(&edited);
    assert_eq!(edited_json["result"]["kind"], "edited");
    assert_eq!(edited_json["preview"]["missing_title_count"], 1);

    let estimate = run(
        &library,
        &["collection", "estimate", "--collection", "1", "--json"],
    );
    assert_success(&estimate);
    let estimate_json = parse_json(&estimate);
    assert_eq!(estimate_json["operation"], "estimate");
    assert_eq!(estimate_json["committed"], false);
    assert_eq!(estimate_json["preview"]["predicted_canonical_bytes"], 42);

    let removal_preview = run(
        &library,
        &["collection", "remove", "--collection", "1", "--json"],
    );
    assert_success(&removal_preview);
    assert_eq!(parse_json(&removal_preview)["committed"], false);
    assert_eq!(
        parse_json(&run(&library, &["collection", "list", "--json"]))["collections"][0]["status"],
        "active"
    );

    let removed = run(
        &library,
        &[
            "collection",
            "remove",
            "--collection",
            "1",
            "--commit",
            "--json",
        ],
    );
    assert_success(&removed);
    assert_eq!(parse_json(&removed)["result"]["kind"], "removed");
    assert!(
        parse_json(&run(&library, &["collection", "list", "--json"]))["collections"]
            .as_array()
            .expect("active collections")
            .is_empty()
    );
    let retained = parse_json(&run(&library, &["collection", "list", "--all", "--json"]));
    assert_eq!(retained["collections"][0]["status"], "tombstoned");
    assert_eq!(retained["collections"][0]["name"], "Renamed");

    let (_, requests) = server.finish();
    assert_eq!(requests.len(), 3);
}

#[test]
fn over_budget_commit_is_refused_after_complete_preview() {
    let temporary = tempfile::tempdir().expect("temporary parent");
    let library = temporary.path().join("library");
    assert_success(&run(&library, &["init"]));
    let server = FixtureServer::start(vec![FixtureResponse::json(TITLE_RESOLUTION)]);
    assert_success(&run(
        &library,
        &[
            "source",
            "add",
            "--api-endpoint",
            server.endpoint(),
            "--language",
            "en",
        ],
    ));
    let refused = run(
        &library,
        &[
            "collection",
            "add",
            "--wiki",
            "1",
            "--name",
            "Too small",
            "--title",
            "Rust_programming_language",
            "--max-bytes",
            "41",
            "--commit",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("exceeds a configured hard budget"));
    assert!(
        parse_json(&run(&library, &["collection", "list", "--json"]))["collections"]
            .as_array()
            .expect("collections")
            .is_empty()
    );
    let (_, requests) = server.finish();
    assert_eq!(requests.len(), 1);
}

#[test]
fn stale_direct_edit_is_rejected_without_overwriting_concurrent_changes() {
    let temporary = tempfile::tempdir().expect("temporary parent");
    let library_root = temporary.path().join("library");
    assert_success(&run(&library_root, &["init"]));
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
            let WriterAccess::Direct(_lease) =
                WriterAccess::discover(&hook_root).expect("discover direct writer")
            else {
                panic!("test mutation unexpectedly found a daemon");
            };
            Library::open(&hook_root)
                .expect("open concurrent writer")
                .rename_collection(
                    wikisync_core::CollectionId::new(1).expect("collection"),
                    "Concurrent rename",
                )
                .expect("concurrent rename");
        },
    );
    assert_success(&run(
        &library_root,
        &[
            "source",
            "add",
            "--api-endpoint",
            server.endpoint(),
            "--language",
            "en",
        ],
    ));
    assert_success(&run(
        &library_root,
        &[
            "collection",
            "add",
            "--wiki",
            "1",
            "--name",
            "Original",
            "--title",
            "Rust_programming_language",
            "--title",
            "Definitely missing WikiSyncer fixture page",
            "--commit",
        ],
    ));

    let stale = run(
        &library_root,
        &[
            "collection",
            "edit",
            "--collection",
            "1",
            "--name",
            "Stale replacement",
            "--title",
            "Rust_programming_language",
            "--title",
            "Definitely missing WikiSyncer fixture page",
            "--history",
            "complete",
            "--commit",
        ],
    );
    assert!(!stale.status.success());
    let error = String::from_utf8_lossy(&stale.stderr);
    assert!(error.contains("no changes were committed"), "{error}");
    assert!(error.contains("re-run collection edit"), "{error}");
    assert!(error.contains("re-preview"), "{error}");

    let library = Library::open_read_only(&library_root).expect("read library");
    let configuration = library
        .collection_configuration(wikisync_core::CollectionId::new(1).expect("collection"))
        .expect("configuration query")
        .expect("configuration");
    assert_eq!(configuration.name, "Concurrent rename");
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

fn run(library: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .arg("--library")
        .arg(library)
        .args(arguments)
        .output()
        .expect("run wikisync")
}

fn parse_json(output: &Output) -> Value {
    assert_success(output);
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
