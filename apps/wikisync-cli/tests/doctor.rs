use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use wikisync_core::{PageId, PageTitle, RevisionId};
use wikisync_store::{CurrentRevisionCapture, Library, SyncRunKind};

const ENDPOINT_SECRET: &str = "SENTINEL_ENDPOINT_SECRET";
const COLLECTION_SECRET: &str = "SENTINEL_COLLECTION_SECRET";
const TITLE_SECRET: &str = "SENTINEL_TITLE_SECRET";
const CONTENT_SECRET: &str = "SENTINEL_CONTENT_SECRET";
const ERROR_SECRET: &str = "SENTINEL_ERROR_MESSAGE_SECRET";
const PATH_SECRET: &str = "SENTINEL_LIBRARY_PATH_SECRET";
const ENVIRONMENT_SECRET: &str = "SENTINEL_ENVIRONMENT_SECRET";

struct DoctorFixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    listener: TcpListener,
}

impl DoctorFixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join(PATH_SECRET);
        fs::create_dir(&root).expect("library directory");
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback sentinel listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let endpoint = format!(
            "http://{}/{ENDPOINT_SECRET}/w/api.php",
            listener.local_addr().expect("listener address")
        );

        let mut library = Library::open(&root).expect("initialize library");
        let wiki_id = library.register_wiki(&endpoint, "en").expect("wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, COLLECTION_SECRET)
            .expect("collection");
        let page_id = PageId::new(25_357_340).expect("page ID");
        let title = PageTitle::new(TITLE_SECRET).expect("title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id,
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(1_300_000_001).expect("revision ID"),
                    parent_id: None,
                    timestamp: "2026-08-21T10:00:00Z",
                    author: Some("SENTINEL_AUTHOR_SECRET"),
                    author_id: Some(42),
                    comment: Some("SENTINEL_COMMENT_SECRET"),
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: CONTENT_SECRET.as_bytes(),
                },
            )
            .expect("capture sentinel content");
        let run = library
            .start_or_resume_sync_run(
                wiki_id,
                Some(collection_id),
                SyncRunKind::Reconciliation,
                1_776_945_600,
            )
            .expect("start run");
        library
            .enqueue_sync_job(
                run.status.run_id,
                "page:fixture",
                "fixture",
                Some(TITLE_SECRET),
            )
            .expect("enqueue job");
        let job = library
            .claim_next_sync_job(run.status.run_id)
            .expect("claim job")
            .expect("job");
        library
            .fail_sync_job(job.job_id, "source-timeout", ERROR_SECRET, true)
            .expect("record redaction sentinel");
        drop(library);

        Self {
            _temporary: temporary,
            root,
            listener,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wikisync"));
        command
            .args(["--library"])
            .arg(&self.root)
            .arg("doctor")
            .env("WIKISYNC_TEST_SECRET", ENVIRONMENT_SECRET);
        command
    }

    fn assert_no_network(&self) {
        let error = self
            .listener
            .accept()
            .expect_err("doctor must not contact the registered source");
        assert_eq!(error.kind(), ErrorKind::WouldBlock);
    }
}

#[test]
fn doctor_json_and_human_output_are_offline_redacted_and_bounded() {
    let fixture = DoctorFixture::new();
    let json_output = fixture
        .command()
        .arg("--json")
        .output()
        .expect("run JSON doctor");
    assert_success(&json_output);
    fixture.assert_no_network();
    let report: Value = serde_json::from_slice(&json_output.stdout).expect("valid doctor JSON");
    assert_eq!(report.pointer("/format/version"), Some(&Value::from(1)));
    assert_eq!(
        report.pointer("/quick_logical_object_verification/data/scope"),
        Some(&Value::from("quick"))
    );
    assert_eq!(
        report.pointer("/recent_runs/data/recent_errors/0/code"),
        Some(&Value::from("source-timeout"))
    );
    assert_redacted(&report);
    assert_strict_keys(&report);
    let encoded = serde_json::to_string(&report).expect("encode report");
    for overclaim in ["manifest", "trusted", "whole_archive", "whole-archive"] {
        assert!(!encoded.to_lowercase().contains(overclaim));
    }

    let human = fixture.command().output().expect("run human doctor");
    assert_success(&human);
    fixture.assert_no_network();
    let human = String::from_utf8(human.stdout).expect("UTF-8 human output");
    assert!(human.contains("WikiSyncer doctor"));
    assert!(human.contains("Quick logical-object verification"));
    for secret in secrets() {
        assert!(!human.contains(secret), "human output leaked {secret}");
    }
}

#[test]
fn bundle_is_private_valid_json_and_never_overwritten() {
    let fixture = DoctorFixture::new();
    let bundle = fixture.root.parent().expect("parent").join("doctor.json");
    let output = fixture
        .command()
        .args(["--json", "--bundle"])
        .arg(&bundle)
        .output()
        .expect("create bundle");
    assert_success(&output);
    let original = fs::read(&bundle).expect("bundle bytes");
    let report: Value = serde_json::from_slice(&original).expect("one valid JSON document");
    assert_redacted(&report);
    assert_eq!(
        fs::metadata(&bundle)
            .expect("bundle metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let refused = fixture
        .command()
        .args(["--bundle"])
        .arg(&bundle)
        .output()
        .expect("refuse overwrite");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("already exists"));
    assert_eq!(fs::read(&bundle).expect("unchanged bundle"), original);
    fixture.assert_no_network();
}

#[test]
fn corrupt_database_still_emits_structured_partial_diagnostics() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path().join(PATH_SECRET);
    fs::create_dir(&root).expect("library root");
    fs::write(root.join("library.sqlite3"), ERROR_SECRET).expect("corrupt database fixture");

    let output = doctor_command(&root)
        .arg("--json")
        .output()
        .expect("run doctor on corrupt database");
    assert_success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).expect("valid partial JSON");
    assert_eq!(report.pointer("/storage/status"), Some(&Value::from("ok")));
    assert_eq!(
        report.pointer("/catalog/status"),
        Some(&Value::from("error"))
    );
    assert_eq!(
        report.pointer("/quick_logical_object_verification/status"),
        Some(&Value::from("error"))
    );
    assert_redacted(&report);
}

fn doctor_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wikisync"));
    command.args(["--library"]).arg(root).arg("doctor");
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn secrets() -> [&'static str; 7] {
    [
        ENDPOINT_SECRET,
        COLLECTION_SECRET,
        TITLE_SECRET,
        CONTENT_SECRET,
        ERROR_SECRET,
        PATH_SECRET,
        ENVIRONMENT_SECRET,
    ]
}

fn assert_redacted(value: &Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                for secret in secrets() {
                    assert!(!key.contains(secret), "JSON key leaked {secret}");
                }
                assert_redacted(value);
            }
        }
        Value::Array(values) => values.iter().for_each(assert_redacted),
        Value::String(value) => {
            for secret in secrets() {
                assert!(!value.contains(secret), "JSON string leaked {secret}");
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn assert_strict_keys(value: &Value) {
    const FORBIDDEN: &[&str] = &[
        "endpoint",
        "title",
        "collection_name",
        "path",
        "message",
        "environment",
        "logs",
        "content",
        "object_id",
        "run_id",
        "wiki_id",
        "collection_id",
        "process_id",
        "findings",
    ];
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                assert!(!FORBIDDEN.contains(&key.as_str()), "forbidden key {key}");
                assert_strict_keys(value);
            }
        }
        Value::Array(values) => values.iter().for_each(assert_strict_keys),
        _ => {}
    }
}
