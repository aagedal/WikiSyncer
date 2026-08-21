use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use wikisync_core::{PageId, PageTitle, RevisionId};
use wikisync_store::{CurrentRevisionCapture, Library};

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
fn historical_selection_fails_clearly_without_changing_output() {
    let directory = tempfile::tempdir().expect("temporary library");
    let root = directory.path();
    seed_library(root);
    let initial = run(root, &["export", "--format", "markdown"]);
    assert_success(&initial);
    let before = fs::read(root.join("exports/current/manifest.json")).unwrap();

    let output = run(
        root,
        &[
            "export",
            "--format",
            "markdown",
            "--at",
            "2026-01-01T00:00:00Z",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("historical export selection with --at is not implemented")
    );
    assert_eq!(
        fs::read(root.join("exports/current/manifest.json")).unwrap(),
        before
    );
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
