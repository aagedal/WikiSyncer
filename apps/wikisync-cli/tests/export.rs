use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use wikisync_core::{PageId, PageTitle, RevisionId};
use wikisync_store::{CurrentRevisionCapture, Library, RevisionCapture};

const ARTICLE_SOURCE: &str = include_str!("../../../fixtures/content/article.wiki");

#[test]
fn exports_current_collection_offline_with_stable_provenance_files() {
    let directory = tempfile::tempdir().expect("temporary library");
    let root = directory.path();
    let collection_id = seed_library(root);

    let first = run(
        root,
        &[
            "export",
            "--format",
            "markdown",
            "--collection",
            &collection_id.to_string(),
        ],
    );
    assert_success(&first);
    let current = root.join("exports/current");
    let article_path = current.join("articles/10-rust-memory-safety.md");
    let article = fs::read_to_string(&article_path).expect("Markdown article");
    assert!(article.contains("wiki: \"en\""));
    assert!(article.contains("page_id: 10"));
    assert!(article.contains("revision_id: 100"));
    assert!(article.contains("revision_time: \"2026-08-19T12:34:56Z\""));
    assert!(article.contains("content_hash: \"b3:"));
    assert!(article.contains(
        "source_url: \"https://example.invalid/w/index.php?title=Rust%20Memory%20Safety&oldid=100\""
    ));
    assert!(article.contains("## Source and attribution"));
    assert!(article.contains("Revision author: Fixture editor."));
    assert!(article.contains("license metadata is not available"));

    let index_bytes = fs::read(current.join("index.jsonl")).expect("index");
    let index_text = String::from_utf8(index_bytes.clone()).expect("UTF-8 index");
    assert_eq!(index_text.lines().count(), 1);
    let index: Value = serde_json::from_str(index_text.trim()).expect("index row");
    assert_eq!(index["relative_path"], "articles/10-rust-memory-safety.md");
    assert_eq!(index["wiki"], "en");
    assert_eq!(index["page_id"], 10);
    assert_eq!(index["revision_id"], 100);
    assert_eq!(index["author"], "Fixture editor");
    assert_eq!(index["transformer_version"], "wikitext-markdown-v1");

    let manifest_bytes = fs::read(current.join("manifest.json")).expect("manifest");
    let manifest: Value = serde_json::from_slice(&manifest_bytes).expect("manifest JSON");
    assert_eq!(manifest["schema"], "wikisync-current-export-v1");
    assert_eq!(manifest["format"], "markdown");
    assert_eq!(manifest["scope"]["kind"], "collection");
    assert_eq!(manifest["scope"]["collection_id"], collection_id);
    assert_eq!(manifest["article_count"], 1);
    assert_eq!(manifest["uncaptured_page_count"], 0);

    let second = run(
        root,
        &[
            "export",
            "--format",
            "markdown",
            "--collection",
            &collection_id.to_string(),
        ],
    );
    assert_success(&second);
    assert_eq!(
        fs::read(&article_path).expect("second article"),
        article.as_bytes()
    );
    assert_eq!(fs::read(current.join("index.jsonl")).unwrap(), index_bytes);
    assert_eq!(
        fs::read(current.join("manifest.json")).unwrap(),
        manifest_bytes
    );

    let text = run(root, &["export", "--format", "text"]);
    assert_success(&text);
    assert!(
        !article_path.exists(),
        "replacement must not retain stale Markdown"
    );
    let text_article = fs::read_to_string(current.join("articles/10-rust-memory-safety.txt"))
        .expect("text article");
    assert!(text_article.contains("SOURCE AND ATTRIBUTION"));
    assert!(text_article.contains("Content hash: b3:"));
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(current.join("manifest.json")).unwrap()).unwrap()
            ["format"],
        "text"
    );
}

#[test]
fn historical_time_slice_selects_each_pages_newest_eligible_revision() {
    let directory = tempfile::tempdir().expect("temporary library");
    let root = directory.path();
    let collection_id = seed_library_with_history(root);
    let initial = run(root, &["export", "--format", "markdown"]);
    assert_success(&initial);
    let before = fs::read(root.join("exports/current/manifest.json")).unwrap();

    let output = run(
        root,
        &[
            "export",
            "--format",
            "markdown",
            "--collection",
            &collection_id.to_string(),
            "--at",
            "2026-08-19T12:15:00Z",
        ],
    );
    assert_success(&output);
    let historical = root.join("exports/at-time-unix-1787141700-markdown-collection-1");
    let first = fs::read_to_string(historical.join("articles/10-first-page.md")).unwrap();
    let second = fs::read_to_string(historical.join("articles/20-second-page.md")).unwrap();
    assert!(first.contains("revision_id: 102"));
    assert!(first.contains("First page at noon."));
    assert!(second.contains("revision_id: 201"));
    assert!(second.contains("Second page initial."));
    let manifest: Value =
        serde_json::from_slice(&fs::read(historical.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema"], "wikisync-historical-export-v1");
    assert_eq!(manifest["at"]["kind"], "timestamp");
    assert_eq!(manifest["at"]["requested"], "2026-08-19T12:15:00Z");
    assert_eq!(manifest["article_count"], 2);
    assert_eq!(
        fs::read(root.join("exports/current/manifest.json")).unwrap(),
        before
    );
}

#[test]
fn historical_boundaries_offsets_and_revision_anchors_are_deterministic() {
    let directory = tempfile::tempdir().expect("temporary library");
    let root = directory.path();
    seed_library_with_history(root);

    let boundary = run(
        root,
        &["export", "--format", "text", "--at", "2026-08-19T12:00:00Z"],
    );
    assert_success(&boundary);
    let time_output = root.join("exports/at-time-unix-1787140800-text-library");
    let article = fs::read_to_string(time_output.join("articles/10-first-page.txt")).unwrap();
    assert!(
        article.contains("Revision ID: 102"),
        "boundary is inclusive"
    );
    let second = fs::read_to_string(time_output.join("articles/20-second-page.txt")).unwrap();
    assert!(second.contains("Revision ID: 201"));

    let same_instant = run(
        root,
        &[
            "export",
            "--format",
            "text",
            "--at",
            "2026-08-19T14:00:00+02:00",
        ],
    );
    assert_success(&same_instant);
    assert!(
        time_output.is_dir(),
        "equivalent offsets share one destination"
    );

    let other_format = run(
        root,
        &[
            "export",
            "--format",
            "markdown",
            "--at",
            "2026-08-19T12:00:00Z",
        ],
    );
    assert_success(&other_format);
    assert!(time_output.is_dir(), "a different format must not collide");
    assert!(
        root.join("exports/at-time-unix-1787140800-markdown-library")
            .is_dir()
    );

    let revision_anchor = run(root, &["export", "--format", "markdown", "--at", "202"]);
    assert_success(&revision_anchor);
    let anchored = root.join("exports/at-revision-1-202-markdown-library");
    let manifest: Value =
        serde_json::from_slice(&fs::read(anchored.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["at"]["kind"], "revision");
    assert_eq!(manifest["at"]["revision_id"], 202);
    let first = fs::read_to_string(anchored.join("articles/10-first-page.md")).unwrap();
    let second = fs::read_to_string(anchored.join("articles/20-second-page.md")).unwrap();
    assert!(first.contains("revision_id: 102"));
    assert!(second.contains("revision_id: 202"));

    let invalid = run(
        root,
        &["export", "--format", "markdown", "--at", "not-a-time"],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("RFC 3339"));

    let before_history = run(
        root,
        &[
            "export",
            "--format",
            "markdown",
            "--at",
            "2020-01-01T00:00:00Z",
        ],
    );
    assert_success(&before_history);
    let early = root.join("exports/at-time-unix-1577836800-markdown-library");
    let manifest: Value =
        serde_json::from_slice(&fs::read(early.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["article_count"], 0);
    assert_eq!(manifest["uncaptured_page_count"], 2);
}

#[test]
fn failed_rebuild_preserves_the_previous_complete_export() {
    let directory = tempfile::tempdir().expect("temporary library");
    let root = directory.path();
    seed_library(root);
    let initial = run(root, &["export", "--format", "markdown"]);
    assert_success(&initial);
    let manifest_path = root.join("exports/current/manifest.json");
    let article_path = root.join("exports/current/articles/10-rust-memory-safety.md");
    let manifest_before = fs::read(&manifest_path).unwrap();
    let article_before = fs::read(&article_path).unwrap();

    let object = only_file_below(&root.join("objects/loose/b3"));
    fs::write(object, b"tampered").expect("tamper temporary fixture object");
    let output = run(root, &["export", "--format", "text"]);
    assert!(!output.status.success());
    assert_eq!(fs::read(manifest_path).unwrap(), manifest_before);
    assert_eq!(fs::read(article_path).unwrap(), article_before);
    assert!(
        !root
            .join("exports/current/articles/10-rust-memory-safety.txt")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_export_paths_without_touching_the_target() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("temporary library");
    let root = directory.path();
    seed_library(root);
    let outside = tempfile::tempdir().expect("outside directory");
    fs::write(outside.path().join("marker"), b"untouched").unwrap();
    symlink(outside.path(), root.join("exports")).expect("exports symlink");

    let output = run(root, &["export", "--format", "markdown"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("symbolic link at exports"));
    assert_eq!(
        fs::read(outside.path().join("marker")).unwrap(),
        b"untouched"
    );
    assert!(!outside.path().join("current").exists());
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_historical_destination_without_touching_current_or_target() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("temporary library");
    let root = directory.path();
    seed_library_with_history(root);
    assert_success(&run(root, &["export", "--format", "markdown"]));
    let current_before = fs::read(root.join("exports/current/manifest.json")).unwrap();
    let outside = tempfile::tempdir().expect("outside directory");
    fs::write(outside.path().join("marker"), b"untouched").unwrap();
    symlink(
        outside.path(),
        root.join("exports/at-time-unix-1787141700-markdown-library"),
    )
    .unwrap();

    let output = run(
        root,
        &[
            "export",
            "--format",
            "markdown",
            "--at",
            "2026-08-19T12:15:00Z",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("symbolic link"));
    assert_eq!(
        fs::read(outside.path().join("marker")).unwrap(),
        b"untouched"
    );
    assert_eq!(
        fs::read(root.join("exports/current/manifest.json")).unwrap(),
        current_before
    );
}

fn seed_library(root: &Path) -> u64 {
    let mut library = Library::open(root).expect("library");
    let wiki_id = library
        .register_wiki("https://example.invalid/w/api.php", "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Export fixture")
        .expect("collection");
    let title = PageTitle::new("Rust Memory Safety").expect("title");
    library
        .capture_current_revision(
            wiki_id,
            collection_id,
            &CurrentRevisionCapture {
                page_id: PageId::new(10).unwrap(),
                namespace: 0,
                title: &title,
                revision_id: RevisionId::new(100).unwrap(),
                parent_id: Some(RevisionId::new(99).unwrap()),
                timestamp: "2026-08-19T12:34:56Z",
                author: Some("Fixture editor"),
                author_id: Some(42),
                comment: Some("Offline fixture"),
                minor: false,
                upstream_sha1: None,
                content_model: "wikitext",
                source: ARTICLE_SOURCE.as_bytes(),
            },
        )
        .expect("capture");
    collection_id.get()
}

fn seed_library_with_history(root: &Path) -> u64 {
    let mut library = Library::open(root).expect("library");
    let wiki_id = library
        .register_wiki("https://example.invalid/w/api.php", "en")
        .expect("wiki");
    let collection_id = library
        .create_explicit_collection(wiki_id, "Historical export fixture")
        .expect("collection");
    let first_title = PageTitle::new("First Page").unwrap();
    library
        .capture_current_revision(
            wiki_id,
            collection_id,
            &CurrentRevisionCapture {
                page_id: PageId::new(10).unwrap(),
                namespace: 0,
                title: &first_title,
                revision_id: RevisionId::new(103).unwrap(),
                parent_id: Some(RevisionId::new(102).unwrap()),
                timestamp: "2026-08-19T13:00:00Z",
                author: Some("Current editor"),
                author_id: None,
                comment: None,
                minor: false,
                upstream_sha1: None,
                content_model: "wikitext",
                source: b"First page current.",
            },
        )
        .unwrap();
    capture_history(
        &mut library,
        wiki_id,
        PageId::new(10).unwrap(),
        102,
        Some(101),
        "2026-08-19T12:00:00Z",
        b"First page at noon.",
    );
    capture_history(
        &mut library,
        wiki_id,
        PageId::new(10).unwrap(),
        101,
        None,
        "2026-08-19T11:00:00Z",
        b"First page initial.",
    );

    let second_title = PageTitle::new("Second Page").unwrap();
    library
        .capture_current_revision(
            wiki_id,
            collection_id,
            &CurrentRevisionCapture {
                page_id: PageId::new(20).unwrap(),
                namespace: 0,
                title: &second_title,
                revision_id: RevisionId::new(203).unwrap(),
                parent_id: Some(RevisionId::new(202).unwrap()),
                timestamp: "2026-08-19T14:00:00Z",
                author: Some("Current editor"),
                author_id: None,
                comment: None,
                minor: false,
                upstream_sha1: None,
                content_model: "wikitext",
                source: b"Second page current.",
            },
        )
        .unwrap();
    capture_history(
        &mut library,
        wiki_id,
        PageId::new(20).unwrap(),
        202,
        Some(201),
        "2026-08-19T12:30:00Z",
        b"Second page half past noon.",
    );
    capture_history(
        &mut library,
        wiki_id,
        PageId::new(20).unwrap(),
        201,
        None,
        "2026-08-19T10:00:00Z",
        b"Second page initial.",
    );
    collection_id.get()
}

fn capture_history(
    library: &mut Library,
    wiki_id: wikisync_core::WikiId,
    page_id: PageId,
    revision_id: u64,
    parent_id: Option<u64>,
    timestamp: &str,
    source: &[u8],
) {
    library
        .capture_revision(
            wiki_id,
            page_id,
            &RevisionCapture {
                revision_id: RevisionId::new(revision_id).unwrap(),
                parent_id: parent_id.map(|id| RevisionId::new(id).unwrap()),
                timestamp,
                author: Some("Historical editor"),
                author_id: None,
                comment: None,
                minor: false,
                upstream_sha1: None,
                content_model: "wikitext",
                source,
            },
        )
        .unwrap();
}

fn run(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .arg("--library")
        .arg(root)
        .args(arguments)
        .output()
        .expect("run wikisync")
}

fn only_file_below(root: &Path) -> std::path::PathBuf {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).expect("read object directory") {
            let path = entry.expect("object entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    assert_eq!(files.len(), 1, "fixture should create one loose object");
    files.pop().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
