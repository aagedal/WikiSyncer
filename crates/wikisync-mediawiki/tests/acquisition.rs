#[allow(dead_code)]
mod support;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use support::{FixtureResponse, FixtureServer};
use wikisync_mediawiki::{
    ClientConfig, DumpAcquisitionError, DumpAcquisitionLimits, DumpDigest, MediaWikiClient,
    TrustedDumpIndex,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const ARTIFACT_NAME: &str = "enwiki-fixture-pages-meta-current.xml.bz2";
const ARTIFACT: &str = "authenticated fixture dump bytes";

fn client(server: &FixtureServer) -> MediaWikiClient {
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 dump-fixture-tests")
        .expect("valid fixture config")
        .with_max_downloaded_response_bytes_per_run(1024 * 1024)
        .expect("download budget");
    MediaWikiClient::new(config).expect("fixture client")
}

fn digest(bytes: &[u8]) -> DumpDigest {
    DumpDigest::from_hex(&blake3::hash(bytes).to_hex()).expect("BLAKE3 hex")
}

fn index(artifact_path: &str, artifact: &[u8]) -> String {
    format!(
        "{{\"schema\":\"wikisync-current-dump-index-v1\",\"database\":\"enwiki\",\"generated_at\":\"2026-08-23T12:00:00Z\",\"artifacts\":[{{\"kind\":\"pages-meta-current-multistream\",\"path\":\"{artifact_path}\",\"bytes\":{},\"blake3\":\"{}\"}}]}}",
        artifact.len(),
        blake3::hash(artifact).to_hex()
    )
}

fn leaked(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn temporary_directory() -> PathBuf {
    let unique = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "wikisync-dump-acquisition-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&path).expect("create fixture cache");
    path.canonicalize().expect("canonical fixture cache")
}

fn trust(server: &FixtureServer, index: &[u8]) -> TrustedDumpIndex {
    TrustedDumpIndex::new(server.endpoint(), digest(index), "enwiki").expect("trust anchor")
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_index_transitively_verifies_and_caches_artifact() {
    let index = leaked(index(ARTIFACT_NAME, ARTIFACT.as_bytes()));
    let server = FixtureServer::start(vec![
        FixtureResponse::json(index),
        FixtureResponse::json(ARTIFACT),
    ]);
    let cache = temporary_directory();
    let acquired = client(&server)
        .acquire_current_dump_set(
            &trust(&server, index.as_bytes()),
            &cache,
            DumpAcquisitionLimits::default(),
        )
        .await
        .expect("authenticated acquisition");

    assert_eq!(acquired.database_name(), "enwiki");
    assert_eq!(acquired.generated_at(), "2026-08-23T12:00:00Z");
    assert_eq!(acquired.artifacts().len(), 1);
    let artifact = &acquired.artifacts()[0];
    assert_eq!(artifact.length(), ARTIFACT.len() as u64);
    assert_eq!(artifact.digest(), digest(ARTIFACT.as_bytes()));
    assert_eq!(fs::read(artifact.path()).unwrap(), ARTIFACT.as_bytes());
    assert_eq!(
        artifact.open().unwrap().metadata().unwrap().len(),
        artifact.length()
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /w/api.php "));
    assert!(requests[1].starts_with(&format!("GET /w/{ARTIFACT_NAME} ")));
    let mut tampered = ARTIFACT.as_bytes().to_vec();
    tampered[0] ^= 1;
    fs::write(artifact.path(), tampered).expect("same-length tamper");
    assert!(matches!(
        artifact.open(),
        Err(DumpAcquisitionError::CachedArtifactChanged)
    ));
    fs::remove_dir_all(cache).expect("remove fixture cache");
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_only_from_an_exact_authenticated_range() {
    let prefix_length = 14_usize;
    let remaining = &ARTIFACT[prefix_length..];
    let content_range = leaked(format!(
        "bytes {prefix_length}-{}/{}",
        ARTIFACT.len() - 1,
        ARTIFACT.len()
    ));
    let index = leaked(index(ARTIFACT_NAME, ARTIFACT.as_bytes()));
    let server = FixtureServer::start(vec![
        FixtureResponse::json(index),
        FixtureResponse::partial(remaining, content_range),
    ]);
    let cache = temporary_directory();
    let partial = cache.join(format!(".{ARTIFACT_NAME}.wikisync.part"));
    fs::write(&partial, &ARTIFACT.as_bytes()[..prefix_length]).expect("seed partial");

    let acquired = client(&server)
        .acquire_current_dump_set(
            &trust(&server, index.as_bytes()),
            &cache,
            DumpAcquisitionLimits::default(),
        )
        .await
        .expect("range resume");
    assert_eq!(
        fs::read(acquired.artifacts()[0].path()).unwrap(),
        ARTIFACT.as_bytes()
    );
    assert!(!partial.exists(), "published partial is removed");
    let requests = server.finish();
    assert!(requests[1].contains(&format!("range: bytes={prefix_length}-")));
    fs::remove_dir_all(cache).expect("remove fixture cache");
}

#[tokio::test(flavor = "multi_thread")]
async fn fails_closed_before_artifact_contact_for_bad_trust_or_path() {
    let valid_index = leaked(index(ARTIFACT_NAME, ARTIFACT.as_bytes()));
    let server = FixtureServer::start(vec![FixtureResponse::json(valid_index)]);
    let cache = temporary_directory();
    let bad_digest = digest(b"different index");
    let bad_trust = TrustedDumpIndex::new(server.endpoint(), bad_digest, "enwiki").unwrap();
    let error = client(&server)
        .acquire_current_dump_set(&bad_trust, &cache, DumpAcquisitionLimits::default())
        .await
        .expect_err("untrusted index");
    assert!(matches!(error, DumpAcquisitionError::IndexDigestMismatch));
    assert_eq!(server.finish().len(), 1);
    fs::remove_dir_all(&cache).expect("remove fixture cache");

    let unsafe_index = leaked(index("../escape.xml.bz2", ARTIFACT.as_bytes()));
    let server = FixtureServer::start(vec![FixtureResponse::json(unsafe_index)]);
    let cache = temporary_directory();
    let error = client(&server)
        .acquire_current_dump_set(
            &trust(&server, unsafe_index.as_bytes()),
            &cache,
            DumpAcquisitionLimits::default(),
        )
        .await
        .expect_err("unsafe artifact path");
    assert!(matches!(error, DumpAcquisitionError::InvalidArtifactPath));
    assert_eq!(server.finish().len(), 1);
    fs::remove_dir_all(cache).expect("remove fixture cache");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_cross_origin_index_redirect_without_following_it() {
    let server = FixtureServer::start(vec![FixtureResponse::redirect(
        "http://localhost:9/secret-index.json".to_owned(),
    )]);
    let cache = temporary_directory();
    let trust = TrustedDumpIndex::new(server.endpoint(), digest(b"unused"), "enwiki").unwrap();
    let error = client(&server)
        .acquire_current_dump_set(&trust, &cache, DumpAcquisitionLimits::default())
        .await
        .expect_err("redirect leaves approved origin");
    assert!(matches!(error, DumpAcquisitionError::Transport(_)));
    assert_eq!(server.finish().len(), 1);
    fs::remove_dir_all(cache).expect("remove fixture cache");
}

#[tokio::test(flavor = "multi_thread")]
async fn digest_mismatch_never_publishes_and_resets_the_poisoned_partial() {
    let mut indexed_bytes = ARTIFACT.as_bytes().to_vec();
    indexed_bytes[0] ^= 1;
    let index = leaked(index(ARTIFACT_NAME, &indexed_bytes));
    let server = FixtureServer::start(vec![
        FixtureResponse::json(index),
        FixtureResponse::json(ARTIFACT),
    ]);
    let cache = temporary_directory();
    let error = client(&server)
        .acquire_current_dump_set(
            &trust(&server, index.as_bytes()),
            &cache,
            DumpAcquisitionLimits::default(),
        )
        .await
        .expect_err("artifact digest mismatch");

    assert!(matches!(
        error,
        DumpAcquisitionError::ArtifactDigestMismatch
    ));
    assert!(!cache.join(ARTIFACT_NAME).exists());
    assert_eq!(
        fs::metadata(cache.join(format!(".{ARTIFACT_NAME}.wikisync.part")))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(server.finish().len(), 2);
    fs::remove_dir_all(cache).expect("remove fixture cache");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_partial_writer_fails_closed_before_artifact_contact() {
    let index = leaked(index(ARTIFACT_NAME, ARTIFACT.as_bytes()));
    let server = FixtureServer::start(vec![FixtureResponse::json(index)]);
    let cache = temporary_directory();
    let partial = cache.join(format!(".{ARTIFACT_NAME}.wikisync.part"));
    let held = fs::File::create(&partial).expect("create held partial");
    fs2::FileExt::lock_exclusive(&held).expect("hold partial lock");

    let error = client(&server)
        .acquire_current_dump_set(
            &trust(&server, index.as_bytes()),
            &cache,
            DumpAcquisitionLimits::default(),
        )
        .await
        .expect_err("concurrent acquisition");
    assert!(matches!(error, DumpAcquisitionError::ConcurrentAcquisition));
    assert_eq!(server.finish().len(), 1);
    drop(held);
    fs::remove_dir_all(cache).expect("remove fixture cache");
}
