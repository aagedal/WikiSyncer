use std::fs;
use std::path::Path;

use tempfile::TempDir;
use wikisync_core::{
    CollectionBudget, CollectionId, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
    ImagePolicy, PageId, PageTitle, TitleSelection, WikiId,
};
use wikisync_store::{
    CollectionSchedule, Library, LogicalObject, NetworkTransferPolicy, ObjectId, ObjectKind,
    ResolvedCollectionMember, ScheduleCadence, StoredCollection, StoredPage, StoredRevision,
    StoredWiki,
};

const LEGACY_SCHEMA_VERSION: u32 = 11;
const CURRENT_SCHEMA_VERSION: u32 = 16;
const FIRST_SOURCE: &[u8] = b"= Alpha =\nAn older beta revision.\n";
const SECOND_SOURCE: &[u8] =
    b"= Alpha =\nThe retained current revision after an offline migration.\n";

#[derive(Debug, Eq, PartialEq)]
struct LegacySnapshot {
    wikis: Vec<StoredWiki>,
    collection: StoredCollection,
    pages: Vec<StoredPage>,
    observed_titles: Vec<PageTitle>,
    revisions: Vec<StoredRevision>,
    revision_sources: Vec<Vec<u8>>,
    logical_objects: Vec<LogicalObject>,
    unresolved_titles: Vec<PageTitle>,
    resolved_members: Vec<ResolvedCollectionMember>,
    schedule: CollectionSchedule,
    network_policy: NetworkTransferPolicy,
}

#[test]
fn older_beta_whole_library_migrates_forward_without_data_loss() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/legacy/beta-schema-11");
    let temporary = TempDir::new().expect("create temporary library parent");
    let library_root = temporary.path().join("library");
    copy_tree(&fixture, &library_root);

    let legacy = Library::open_read_only(&library_root).expect("open schema-11 fixture read-only");
    assert_eq!(
        legacy.schema_version().expect("legacy schema version"),
        LEGACY_SCHEMA_VERSION
    );
    let before = snapshot(&legacy);
    assert_eq!(before.collection.name, "Older beta collection");
    assert_eq!(
        before
            .revisions
            .iter()
            .map(|revision| revision.revision_id.get())
            .collect::<Vec<_>>(),
        [1_002, 1_001]
    );
    assert_eq!(
        before.revision_sources,
        [SECOND_SOURCE.to_vec(), FIRST_SOURCE.to_vec()]
    );
    assert_eq!(before.logical_objects.len(), 2);
    assert_eq!(
        before.schedule,
        CollectionSchedule {
            collection_id: collection_id(),
            cadence: ScheduleCadence::interval(3_600).expect("valid fixture interval"),
            jitter_seconds: 120,
            paused: false,
            next_run_at: Some(1_700_007_200),
            last_started_at: Some(1_700_003_600),
        }
    );
    assert_eq!(
        before.network_policy,
        NetworkTransferPolicy::new(2, Some(4_096), true).expect("valid fixture policy")
    );
    drop(legacy);

    let migrated = Library::open(&library_root).expect("migrate the copied beta library");
    assert_eq!(
        migrated.schema_version().expect("migrated schema version"),
        CURRENT_SCHEMA_VERSION
    );
    assert_eq!(snapshot(&migrated), before);

    let collection_id = collection_id();
    let configuration = migrated
        .collection_configuration(collection_id)
        .expect("read migrated collection configuration")
        .expect("fixture collection remains configured");
    assert_eq!(configuration.name, "Older beta collection");
    assert_eq!(configuration.history_policy, HistoryPolicy::Complete);
    assert_eq!(configuration.image_policy, ImagePolicy::None);
    assert_eq!(
        configuration.removal_policy,
        CollectionRemovalPolicy::StopTrackingRetainHistory
    );
    assert_eq!(
        configuration.budget,
        CollectionBudget::unlimited()
            .with_maximum_pages(100)
            .expect("valid page budget")
            .with_maximum_bytes(1_048_576)
            .expect("valid byte budget")
    );
    assert_eq!(
        configuration.rule,
        CollectionRule::ExplicitTitles(
            TitleSelection::new([
                PageTitle::new("Alpha").expect("valid title"),
                PageTitle::new("Never Resolved").expect("valid title"),
            ])
            .expect("non-empty title selection")
        )
    );

    let estimate = migrated
        .collection_estimate(collection_id)
        .expect("read migrated collection estimate");
    assert_eq!(estimate.resolved_page_count, 1);
    assert_eq!(estimate.current_canonical_bytes, 102);
    assert_eq!(estimate.predicted_canonical_bytes, Some(102));
    assert_eq!(estimate.predicted_at, Some(1_700_000_500));
    assert_eq!(migrated.manifest_count().expect("manifest inventory"), 0);
    assert!(
        migrated
            .dump_import_status(1)
            .expect("new dump-import table")
            .is_none()
    );

    drop(migrated);
    let reopened = Library::open(&library_root).expect("reopen the migrated library");
    assert_eq!(
        reopened.schema_version().expect("reopened schema version"),
        CURRENT_SCHEMA_VERSION
    );
    assert_eq!(snapshot(&reopened), before);
}

fn snapshot(library: &Library) -> LegacySnapshot {
    let wiki_id = wiki_id();
    let collection_id = collection_id();
    let page_id = page_id();
    let revisions = library
        .revisions_for_page(wiki_id, page_id)
        .expect("list fixture revisions");
    let revision_sources = revisions
        .iter()
        .map(|revision| {
            library
                .read_object(revision.content_object_id)
                .expect("read and verify retained canonical source")
        })
        .collect();

    LegacySnapshot {
        wikis: library.wikis().expect("list fixture wikis"),
        collection: library
            .collection(collection_id)
            .expect("read fixture collection")
            .expect("fixture collection exists"),
        pages: library
            .collection_pages(wiki_id, collection_id)
            .expect("list fixture pages"),
        observed_titles: library
            .page_titles(wiki_id, page_id)
            .expect("list retained titles"),
        revisions,
        revision_sources,
        logical_objects: library
            .logical_objects_after(None, 10)
            .expect("list logical objects"),
        unresolved_titles: library
            .unresolved_titles(collection_id)
            .expect("list unresolved titles"),
        resolved_members: library
            .resolved_collection_members(collection_id)
            .expect("list resolved members"),
        schedule: library
            .collection_schedule(collection_id)
            .expect("read schedule")
            .expect("fixture schedule exists"),
        network_policy: library
            .network_transfer_policy()
            .expect("read network policy"),
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir(destination).expect("create fixture copy root");
    for entry in fs::read_dir(source).expect("read fixture directory") {
        let entry = entry.expect("read fixture entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("read fixture entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy fixture file");
        }
    }
}

fn wiki_id() -> WikiId {
    WikiId::new(1).expect("valid fixture wiki ID")
}

fn collection_id() -> CollectionId {
    CollectionId::new(1).expect("valid fixture collection ID")
}

fn page_id() -> PageId {
    PageId::new(42).expect("valid fixture page ID")
}

#[test]
fn fixture_object_identities_are_stable() {
    assert_eq!(
        ObjectId::for_bytes(ObjectKind::Wikitext, FIRST_SOURCE).to_string(),
        "b3:f7439b509b1242d1a11877ced9a4b4d38758c2c8374164cee810d169fa164c06"
    );
    assert_eq!(
        ObjectId::for_bytes(ObjectKind::Wikitext, SECOND_SOURCE).to_string(),
        "b3:3c75e33ae3cc10e8c0e55c97a575e7acf8909947228288ce09bffa5e4b18dc38"
    );
}
