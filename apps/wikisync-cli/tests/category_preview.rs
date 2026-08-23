mod support;

use std::process::{Command, Output};

use serde_json::Value;
use support::{FixtureResponse, FixtureServer};

const CATEGORY_MEMBERS_PAGE_1: &str =
    include_str!("../../../fixtures/mediawiki/category-members-page-1.json");
const CATEGORY_MEMBERS_PAGE_2: &str =
    include_str!("../../../fixtures/mediawiki/category-members-page-2.json");
const CATEGORY_MEMBERS_SUBCATEGORY: &str =
    include_str!("../../../fixtures/mediawiki/category-members-subcategory.json");

#[test]
fn category_preview_is_standalone_bounded_and_machine_readable() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(CATEGORY_MEMBERS_PAGE_1),
        FixtureResponse::json(CATEGORY_MEMBERS_PAGE_2),
        FixtureResponse::json(CATEGORY_MEMBERS_SUBCATEGORY),
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "category-preview",
            "--api-endpoint",
            server.endpoint(),
            "--depth",
            "1",
            "--json",
            "Category:Root",
        ])
        .output()
        .expect("run category preview");
    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).expect("preview JSON");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["root"], "Category:Root");
    assert_eq!(value["recursion_depth"], 1);
    assert_eq!(value["page_count"], 3);
    assert_eq!(value["category_count"], 2);
    assert_eq!(value["batches"], 3);
    assert_eq!(value["limits"]["max_pages"], 10_000);
    assert_eq!(value["pages"][0]["title"], "Alpha");
    assert_eq!(value["pages"][2]["title"], "Gamma");

    let (_, requests) = server.finish();
    assert_eq!(requests.len(), 3);
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "category preview failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
