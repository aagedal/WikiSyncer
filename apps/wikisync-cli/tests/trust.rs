#[path = "../src/trust.rs"]
mod trust;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use trust::{
    AnchorComparison, AnchorWriteMode, MAX_SIGNING_KEY_BYTES, RotationPhase, TrustError,
    export_current_trusted_head, generate_signing_key, import_signing_key, inspect_trusted_head,
    rotate_signing_key, validate_signing_key,
};
use wikisync_store::{Library, SyncRunKind};

struct TrustFixture {
    _temporary: tempfile::TempDir,
    library_root: PathBuf,
    external_root: PathBuf,
}

impl TrustFixture {
    fn new(manifest_count: u64) -> Self {
        let temporary = tempfile::tempdir().expect("temporary fixture");
        let library_root = temporary.path().join("library");
        let external_root = temporary.path().join("external-trust");
        fs::create_dir(&external_root).expect("external trust directory");
        fs::set_permissions(&external_root, fs::Permissions::from_mode(0o700))
            .expect("private external trust directory");
        let mut library = Library::open(&library_root).expect("library");
        let wiki_id = library
            .register_wiki("https://example.invalid/w/api.php", "fixture")
            .expect("fixture wiki");
        for sequence in 0..manifest_count {
            let run = library
                .start_or_resume_sync_run(
                    wiki_id,
                    None,
                    if sequence == 0 {
                        SyncRunKind::Bootstrap
                    } else {
                        SyncRunKind::Update
                    },
                    100 + sequence * 10,
                )
                .expect("fixture run")
                .status;
            library
                .complete_sync_run(run.run_id, None)
                .expect("complete fixture run");
            library
                .append_sync_manifest(run.run_id)
                .expect("fixture manifest");
        }
        drop(library);
        Self {
            _temporary: temporary,
            library_root,
            external_root,
        }
    }

    fn library(&self) -> Library {
        Library::open_read_only(&self.library_root).expect("read-only library")
    }

    fn external(&self, name: &str) -> PathBuf {
        self.external_root.join(name)
    }

    fn append_manifest(&self) {
        let mut library = Library::open(&self.library_root).expect("writable library");
        let wiki_id = library.wikis().expect("wikis")[0].wiki_id;
        let run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 1_000)
            .expect("additional run")
            .status;
        library
            .complete_sync_run(run.run_id, None)
            .expect("complete additional run");
        library
            .append_sync_manifest(run.run_id)
            .expect("additional manifest");
    }

    fn complete_run_without_manifest(&self) {
        let mut library = Library::open(&self.library_root).expect("writable library");
        let wiki_id = library.wikis().expect("wikis")[0].wiki_id;
        let run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 2_000)
            .expect("unmanifested run")
            .status;
        library
            .complete_sync_run(run.run_id, None)
            .expect("complete unmanifested run");
    }
}

#[test]
fn generated_and_imported_keys_are_private_external_and_create_new() {
    let fixture = TrustFixture::new(1);
    let generated = fixture.external("generated.pk8");
    let imported = fixture.external("imported.pk8");

    let generated_summary =
        generate_signing_key(&fixture.library_root, &generated).expect("generate key");
    assert!(generated_summary.byte_length > 0);
    assert_eq!(
        fs::metadata(&generated)
            .expect("key metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    validate_signing_key(&fixture.library_root, &generated).expect("validate generated key");

    let imported_summary = import_signing_key(&fixture.library_root, &generated, &imported)
        .expect("import existing key");
    assert_eq!(generated_summary, imported_summary);
    assert_eq!(fs::read(&generated).unwrap(), fs::read(&imported).unwrap());
    assert!(generated.exists(), "import must retain the source key");
    assert!(matches!(
        generate_signing_key(&fixture.library_root, &generated),
        Err(TrustError::AlreadyExists(_))
    ));
    assert!(matches!(
        import_signing_key(&fixture.library_root, &generated, &imported),
        Err(TrustError::AlreadyExists(_))
    ));
}

#[test]
fn key_paths_reject_library_storage_symlinks_and_public_permissions() {
    let fixture = TrustFixture::new(1);
    let inside_library = fixture.library_root.join("sole-key.pk8");
    assert!(matches!(
        generate_signing_key(&fixture.library_root, &inside_library),
        Err(TrustError::UnsafePath(_))
    ));

    let key = fixture.external("key.pk8");
    generate_signing_key(&fixture.library_root, &key).expect("generate key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).expect("loosen permissions");
    assert!(matches!(
        validate_signing_key(&fixture.library_root, &key),
        Err(TrustError::InsecurePermissions(_))
    ));

    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let linked = fixture.external("linked.pk8");
    symlink(&key, &linked).expect("key symlink");
    assert!(matches!(
        validate_signing_key(&fixture.library_root, &linked),
        Err(TrustError::UnsafePath(_))
    ));

    let public_parent = fixture._temporary.path().join("public-parent");
    fs::create_dir(&public_parent).unwrap();
    fs::set_permissions(&public_parent, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        generate_signing_key(&fixture.library_root, &public_parent.join("key.pk8")),
        Err(TrustError::InsecurePermissions(_))
    ));
}

#[test]
fn trusted_head_export_inspection_staleness_and_refresh_are_explicit() {
    let fixture = TrustFixture::new(1);
    let key = fixture.external("key.pk8");
    let anchor = fixture.external("trusted-head.json");
    generate_signing_key(&fixture.library_root, &key).expect("generate key");
    let library = fixture.library();
    let first = export_current_trusted_head(&library, &key, &anchor, AnchorWriteMode::CreateNew)
        .expect("create anchor");
    assert_eq!(first.sequence, 1);
    assert_eq!(
        inspect_trusted_head(&library, &anchor)
            .expect("inspect current")
            .comparison,
        AnchorComparison::AuthenticatedCurrent
    );
    drop(library);

    fixture.append_manifest();
    let library = fixture.library();
    let stale = inspect_trusted_head(&library, &anchor).expect("inspect stale anchor");
    assert_eq!(stale.comparison, AnchorComparison::DifferentHead);
    assert!(!stale.report.trusted_head_authenticated);
    let refreshed =
        export_current_trusted_head(&library, &key, &anchor, AnchorWriteMode::RefreshExisting)
            .expect("refresh anchor");
    assert_eq!(refreshed.sequence, 2);
    assert_eq!(
        inspect_trusted_head(&library, &anchor)
            .expect("inspect refreshed")
            .comparison,
        AnchorComparison::AuthenticatedCurrent
    );
    assert_eq!(
        fs::metadata(&anchor).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn refresh_refuses_to_overwrite_an_unrecognized_file() {
    let fixture = TrustFixture::new(1);
    let key = fixture.external("key.pk8");
    let anchor = fixture.external("do-not-overwrite.txt");
    generate_signing_key(&fixture.library_root, &key).expect("generate key");
    fs::write(&anchor, b"unrelated user data").expect("sentinel file");
    fs::set_permissions(&anchor, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&anchor).unwrap();

    assert!(matches!(
        export_current_trusted_head(
            &fixture.library(),
            &key,
            &anchor,
            AnchorWriteMode::RefreshExisting,
        ),
        Err(TrustError::InvalidTrustedHead)
    ));
    assert_eq!(fs::read(&anchor).unwrap(), before);
}

#[test]
fn export_fails_before_write_when_full_verification_is_not_clean() {
    let fixture = TrustFixture::new(1);
    let key = fixture.external("key.pk8");
    let anchor = fixture.external("trusted-head.json");
    generate_signing_key(&fixture.library_root, &key).expect("generate key");
    export_current_trusted_head(
        &fixture.library(),
        &key,
        &anchor,
        AnchorWriteMode::CreateNew,
    )
    .expect("initial verified anchor");
    let before = fs::read(&anchor).unwrap();

    // A successful run without its required manifest is a full-verification finding
    // that manifest-chain signing alone does not reject.
    fixture.complete_run_without_manifest();
    let error = export_current_trusted_head(
        &fixture.library(),
        &key,
        &anchor,
        AnchorWriteMode::RefreshExisting,
    )
    .expect_err("unclean library must not publish an anchor");
    assert!(matches!(
        error,
        TrustError::LibraryNotFullyVerified { finding_count } if finding_count > 0
    ));
    assert_eq!(fs::read(&anchor).unwrap(), before);
}

#[test]
fn canonical_signature_tampering_is_reported_distinctly() {
    let fixture = TrustFixture::new(1);
    let key = fixture.external("key.pk8");
    let anchor = fixture.external("anchor.json");
    generate_signing_key(&fixture.library_root, &key).expect("generate key");
    export_current_trusted_head(
        &fixture.library(),
        &key,
        &anchor,
        AnchorWriteMode::CreateNew,
    )
    .expect("anchor");
    let mut bytes = fs::read(&anchor).unwrap();
    let signature_start = bytes
        .windows(b"\"signature\":\"".len())
        .position(|window| window == b"\"signature\":\"")
        .expect("signature field")
        + b"\"signature\":\"".len();
    bytes[signature_start] = if bytes[signature_start] == b'0' {
        b'1'
    } else {
        b'0'
    };
    fs::write(&anchor, bytes).unwrap();
    fs::set_permissions(&anchor, fs::Permissions::from_mode(0o600)).unwrap();

    let inspection = inspect_trusted_head(&fixture.library(), &anchor).expect("inspect tampering");
    assert_eq!(inspection.comparison, AnchorComparison::InvalidSignature);
}

#[test]
fn rotation_keeps_a_durable_recovery_anchor_and_never_deletes_old_key() {
    let fixture = TrustFixture::new(2);
    let old_key = fixture.external("old-key.pk8");
    let current_anchor = fixture.external("current-anchor.json");
    let new_key = fixture.external("new-key.pk8");
    let recovery_anchor = fixture.external("recovery-anchor.json");
    generate_signing_key(&fixture.library_root, &old_key).expect("old key");
    export_current_trusted_head(
        &fixture.library(),
        &old_key,
        &current_anchor,
        AnchorWriteMode::CreateNew,
    )
    .expect("old anchor");
    let old_anchor_bytes = fs::read(&current_anchor).unwrap();

    let rotation = rotate_signing_key(
        &fixture.library(),
        &current_anchor,
        &new_key,
        &recovery_anchor,
    )
    .expect("rotate key");
    assert_ne!(rotation.previous.public_key, rotation.current.public_key);
    assert_eq!(rotation.previous.sequence, rotation.current.sequence);
    assert_eq!(fs::read(&recovery_anchor).unwrap(), old_anchor_bytes);
    assert!(
        old_key.exists(),
        "rotation must not delete the previous key"
    );
    validate_signing_key(&fixture.library_root, &new_key).expect("new key is valid");
    assert_eq!(
        inspect_trusted_head(&fixture.library(), &current_anchor)
            .expect("new current anchor")
            .comparison,
        AnchorComparison::AuthenticatedCurrent
    );
    assert_eq!(
        inspect_trusted_head(&fixture.library(), &recovery_anchor)
            .expect("recovery anchor")
            .comparison,
        AnchorComparison::AuthenticatedCurrent
    );
}

#[test]
fn failed_rotation_reports_preflight_and_leaves_current_anchor_unchanged() {
    let fixture = TrustFixture::new(1);
    let old_key = fixture.external("old-key.pk8");
    let current_anchor = fixture.external("current-anchor.json");
    let new_key = fixture.external("new-key.pk8");
    let recovery_anchor = fixture.external("recovery-anchor.json");
    generate_signing_key(&fixture.library_root, &old_key).expect("old key");
    export_current_trusted_head(
        &fixture.library(),
        &old_key,
        &current_anchor,
        AnchorWriteMode::CreateNew,
    )
    .expect("old anchor");
    fs::write(&recovery_anchor, b"occupied").unwrap();
    fs::set_permissions(&recovery_anchor, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&current_anchor).unwrap();

    let error = rotate_signing_key(
        &fixture.library(),
        &current_anchor,
        &new_key,
        &recovery_anchor,
    )
    .expect_err("occupied recovery path must stop rotation");
    assert_eq!(error.phase, RotationPhase::Preflight);
    assert!(!new_key.exists());
    assert_eq!(fs::read(&current_anchor).unwrap(), before);
}

#[test]
fn errors_do_not_echo_sensitive_paths_or_key_bytes() {
    let fixture = TrustFixture::new(1);
    let secret_name = "SENTINEL_SECRET_KEY_PATH";
    let invalid = fixture.external(secret_name);
    fs::write(&invalid, b"SENTINEL_SECRET_KEY_BYTES").unwrap();
    fs::set_permissions(&invalid, fs::Permissions::from_mode(0o600)).unwrap();
    let error = validate_signing_key(&fixture.library_root, &invalid)
        .expect_err("invalid PKCS#8 must fail")
        .to_string();
    assert!(!error.contains(secret_name));
    assert!(!error.contains("SENTINEL_SECRET_KEY_BYTES"));
}

#[test]
fn relative_paths_are_rejected_as_ambiguous() {
    let fixture = TrustFixture::new(1);
    assert!(matches!(
        generate_signing_key(&fixture.library_root, Path::new("relative.pk8")),
        Err(TrustError::UnsafePath(_))
    ));
}

#[test]
fn signing_key_reads_are_bounded_before_pkcs8_parsing() {
    let fixture = TrustFixture::new(1);
    let oversized = fixture.external("oversized.pk8");
    fs::write(&oversized, vec![0_u8; MAX_SIGNING_KEY_BYTES + 1]).unwrap();
    fs::set_permissions(&oversized, fs::Permissions::from_mode(0o600)).unwrap();

    assert!(matches!(
        validate_signing_key(&fixture.library_root, &oversized),
        Err(TrustError::InputTooLarge("signing key"))
    ));
}
