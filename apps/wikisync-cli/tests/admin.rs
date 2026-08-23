use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const EMPTY_SOURCE_LIST_V1: &[u8] =
    include_bytes!("../../../fixtures/contracts/cli/source-list-empty-v1.json");
const EMPTY_STATUS_V1: &[u8] =
    include_bytes!("../../../fixtures/contracts/cli/status-empty-v1.json");

#[test]
fn initializes_and_lists_registered_sources_and_collections() {
    let directory = tempfile::tempdir().expect("temporary parent");
    let library = directory.path().join("library");

    let initialized = run(&library, &["init"]);
    assert_success(&initialized);
    assert!(library.join("library.sqlite3").is_file());

    let added = run(
        &library,
        &[
            "source",
            "add",
            "--api-endpoint",
            "https://en.wikipedia.org/w/api.php",
            "--language",
            "en",
            "--json",
        ],
    );
    assert_success(&added);
    let added_json: Value = serde_json::from_slice(&added.stdout).expect("source add JSON");
    assert_eq!(added_json["schema_version"], 1);
    assert_eq!(added_json["wiki_id"], 1);
    assert_eq!(added_json["language_code"], "en");
    assert_eq!(added_json["created"], true);

    let sources = run(&library, &["source", "list", "--json"]);
    assert_success(&sources);
    let sources_json: Value = serde_json::from_slice(&sources.stdout).expect("source list JSON");
    assert_eq!(sources_json["schema_version"], 1);
    assert_eq!(sources_json["sources"].as_array().unwrap().len(), 1);
    assert_eq!(sources_json["sources"][0]["wiki_id"], 1);

    let removed = run(&library, &["source", "remove", "--wiki", "1", "--json"]);
    assert_success(&removed);
    let removed_json: Value = serde_json::from_slice(&removed.stdout).expect("source remove JSON");
    assert_eq!(removed_json["schema_version"], 1);
    assert_eq!(removed_json["wiki_id"], 1);
    assert_eq!(removed_json["removed"], true);

    let sources = run(&library, &["source", "list", "--json"]);
    assert_success(&sources);
    assert_eq!(sources.stdout, EMPTY_SOURCE_LIST_V1);
    let sources_json: Value = serde_json::from_slice(&sources.stdout).expect("empty source JSON");
    assert_eq!(sources_json["schema_version"], 1);
    assert!(sources_json["sources"].as_array().unwrap().is_empty());

    let collections = run(&library, &["collection", "list", "--json"]);
    assert_success(&collections);
    let collections_json: Value =
        serde_json::from_slice(&collections.stdout).expect("collection list JSON");
    assert_eq!(collections_json["collections"].as_array().unwrap().len(), 0);

    let status = run(&library, &["status", "--json"]);
    assert_success(&status);
    assert_eq!(status.stdout, EMPTY_STATUS_V1);
    let status_json: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status_json["schema_version"], 1);
    assert_eq!(status_json["state"], "idle");

    let repeated = run(&library, &["init"]);
    assert!(!repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("refusing to initialize"));
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
