//! Dispatch from the daemon contract into the existing application crates.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::runtime::{Builder, Runtime};
use wikisync_core::CollectionId;
use wikisync_integrity::{VerificationScope, verify_library};
use wikisync_mediawiki::{ClientConfig, MediaWikiClient};
use wikisync_store::Library;
use wikisync_sync::{ReconciliationReport, reconcile_collection_heads};

use crate::{
    HandlerStatus, Mutation, MutationOutcome, OperationError, RequestHandler,
    canonical_library_root,
};

/// Production mutation dispatcher backed by the durable WikiSyncer application APIs.
///
/// Scheduling remains intentionally separate. Each call opens the library only after
/// the daemon has acquired the cooperative writer lease, and completes synchronously
/// before another request is accepted.
#[derive(Debug)]
pub struct ApplicationHandler {
    library_root: PathBuf,
    last_operation: Option<String>,
}

impl ApplicationHandler {
    /// Creates a dispatcher for an existing library directory.
    pub fn new(library_root: impl AsRef<Path>) -> Result<Self, crate::DaemonError> {
        Ok(Self {
            library_root: canonical_library_root(library_root.as_ref())?,
            last_operation: None,
        })
    }

    fn runtime() -> Result<Runtime, OperationError> {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| OperationError::failed(format!("cannot start sync runtime: {error}")))
    }

    fn sync_collection(
        runtime: &Runtime,
        library: &mut Library,
        collection_id: CollectionId,
        checkpoint_candidate: u64,
    ) -> Result<ReconciliationReport, OperationError> {
        let configuration = library
            .collection_configuration(collection_id)
            .map_err(operation_failed)?
            .ok_or_else(|| {
                OperationError::failed(format!(
                    "collection {collection_id} has no committed configuration"
                ))
            })?;
        let wiki = library
            .wiki(configuration.wiki_id)
            .map_err(operation_failed)?
            .ok_or_else(|| {
                OperationError::failed(format!(
                    "source wiki {} for collection {collection_id} is missing",
                    configuration.wiki_id
                ))
            })?;
        let client_config = ClientConfig::new(&wiki.api_endpoint, user_agent())
            .map_err(|error| OperationError::failed(error.to_string()))?;
        let client = MediaWikiClient::new(client_config)
            .map_err(|error| OperationError::failed(error.to_string()))?;
        runtime
            .block_on(reconcile_collection_heads(
                &client,
                library,
                configuration.wiki_id,
                collection_id,
                checkpoint_candidate,
            ))
            .map_err(|error| OperationError::failed(error.to_string()))
    }

    fn sync_one(&mut self, raw_collection_id: u64) -> Result<MutationOutcome, OperationError> {
        let collection_id = CollectionId::new(raw_collection_id)
            .map_err(|error| OperationError::failed(error.to_string()))?;
        let mut library = Library::open(&self.library_root).map_err(operation_failed)?;
        let runtime = Self::runtime()?;
        let report = Self::sync_collection(&runtime, &mut library, collection_id, unix_time()?)?;
        let payload = reconciliation_payload(1, &report);
        self.last_operation = Some(format!("synchronized collection {collection_id}"));
        Ok(MutationOutcome {
            result: "synchronization-complete".to_owned(),
            payload: payload.into_bytes(),
        })
    }

    fn sync_all(&mut self) -> Result<MutationOutcome, OperationError> {
        let mut library = Library::open(&self.library_root).map_err(operation_failed)?;
        let collections = library.collections().map_err(operation_failed)?;
        let runtime = Self::runtime()?;
        let checkpoint_candidate = unix_time()?;
        let mut totals = ReconciliationTotals::default();
        for collection in &collections {
            let report = Self::sync_collection(
                &runtime,
                &mut library,
                collection.collection_id,
                checkpoint_candidate,
            )?;
            totals.add(&report);
        }
        let collection_count = collections.len();
        self.last_operation = Some(format!("synchronized {collection_count} collections"));
        Ok(MutationOutcome {
            result: "synchronization-complete".to_owned(),
            payload: totals.payload(collection_count).into_bytes(),
        })
    }

    fn verify(&mut self, full: bool) -> Result<MutationOutcome, OperationError> {
        let library = Library::open(&self.library_root).map_err(operation_failed)?;
        let scope = if full {
            VerificationScope::Full
        } else {
            VerificationScope::Quick
        };
        let report = verify_library(&library, scope)
            .map_err(|error| OperationError::failed(error.to_string()))?;
        let fully_verified = report.is_verified_since_capture();
        let payload = format!(
            "scope={} coverage={:?} objects_examined={} objects_verified={} canonical_bytes_verified={} findings={} omitted_findings={} fully_verified={fully_verified}",
            if full { "full" } else { "quick" },
            report.coverage,
            report.objects_examined,
            report.objects_verified,
            report.canonical_bytes_verified,
            report.finding_count,
            report.omitted_findings,
        );
        self.last_operation = Some(format!(
            "verification completed with {} findings",
            report.finding_count
        ));
        Ok(MutationOutcome {
            // Completion is distinct from a claim that every object verified.
            result: "verification-complete".to_owned(),
            payload: payload.into_bytes(),
        })
    }

    fn compact(&mut self) -> Result<MutationOutcome, OperationError> {
        let mut library = Library::open(&self.library_root).map_err(operation_failed)?;
        let summary = library.pack_loose_objects().map_err(operation_failed)?;
        let (payload, last_operation) = summary.map_or_else(
            || (
                "packed=false reason=no-unpacked-loose-objects".to_owned(),
                "compaction completed without a new pack".to_owned(),
            ),
            |pack| {
                (
                    format!(
                        "packed=true pack_id={} generation={} objects={} full_entries={} delta_entries={} pack_bytes={} index_bytes={}",
                        pack.pack_id,
                        pack.generation,
                        pack.object_count,
                        pack.full_entries,
                        pack.delta_entries,
                        pack.pack_bytes,
                        pack.index_bytes,
                    ),
                    format!("compacted {} objects", pack.object_count),
                )
            },
        );
        self.last_operation = Some(last_operation);
        Ok(MutationOutcome {
            result: "compaction-complete".to_owned(),
            payload: payload.into_bytes(),
        })
    }
}

impl RequestHandler for ApplicationHandler {
    fn status(&self) -> HandlerStatus {
        HandlerStatus {
            state: "idle".to_owned(),
            detail: self
                .last_operation
                .clone()
                .unwrap_or_else(|| "application mutation dispatcher is ready".to_owned()),
        }
    }

    fn mutate(&mut self, mutation: Mutation) -> Result<MutationOutcome, OperationError> {
        match mutation {
            Mutation::SyncAll => self.sync_all(),
            Mutation::SyncCollection(collection_id) => self.sync_one(collection_id),
            Mutation::Verify { full } => self.verify(full),
            Mutation::Compact => self.compact(),
            Mutation::Extension { name, .. } => Err(OperationError::unsupported(format!(
                "extension operation {name:?} is not implemented"
            ))),
        }
    }
}

#[derive(Debug, Default)]
struct ReconciliationTotals {
    pages_checked: usize,
    differing_heads: usize,
    missing_pages: usize,
    revision_batches: usize,
    revisions_enumerated: usize,
    revisions_captured: usize,
    revisions_reused: usize,
    resumed_runs: usize,
}

impl ReconciliationTotals {
    fn add(&mut self, report: &ReconciliationReport) {
        self.pages_checked = self.pages_checked.saturating_add(report.pages_checked);
        self.differing_heads = self.differing_heads.saturating_add(report.differing_heads);
        self.missing_pages = self.missing_pages.saturating_add(report.missing_pages);
        self.revision_batches = self
            .revision_batches
            .saturating_add(report.revision_batches);
        self.revisions_enumerated = self
            .revisions_enumerated
            .saturating_add(report.revisions_enumerated);
        self.revisions_captured = self
            .revisions_captured
            .saturating_add(report.revisions_captured);
        self.revisions_reused = self
            .revisions_reused
            .saturating_add(report.revisions_reused);
        self.resumed_runs = self
            .resumed_runs
            .saturating_add(usize::from(report.resumed));
    }

    fn payload(&self, collections: usize) -> String {
        format!(
            "collections={collections} pages_checked={} differing_heads={} missing_pages={} revision_batches={} revisions_enumerated={} revisions_captured={} revisions_reused={} resumed_runs={}",
            self.pages_checked,
            self.differing_heads,
            self.missing_pages,
            self.revision_batches,
            self.revisions_enumerated,
            self.revisions_captured,
            self.revisions_reused,
            self.resumed_runs,
        )
    }
}

fn reconciliation_payload(collections: usize, report: &ReconciliationReport) -> String {
    let mut totals = ReconciliationTotals::default();
    totals.add(report);
    totals.payload(collections)
}

fn operation_failed(error: impl std::fmt::Display) -> OperationError {
    OperationError::failed(error.to_string())
}

fn unix_time() -> Result<u64, OperationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| OperationError::failed("system clock is before the Unix epoch"))
}

fn user_agent() -> String {
    format!(
        "WikiSyncer-daemon/{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use wikisync_store::ObjectKind;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug)]
    struct TempLibrary(PathBuf);

    impl TempLibrary {
        fn new() -> Self {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_dir()
                .expect("current directory")
                .join(format!(".wsd-app-{}-{unique}", std::process::id()));
            fs::create_dir(&path).expect("create temporary library");
            Library::open(&path).expect("initialize temporary library");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempLibrary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn verification_reports_completion_without_overclaiming_integrity() {
        let temporary = TempLibrary::new();
        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let outcome = handler
            .mutate(Mutation::Verify { full: false })
            .expect("verify empty library");
        assert_eq!(outcome.result, "verification-complete");
        let payload = String::from_utf8(outcome.payload).expect("UTF-8 receipt");
        assert!(payload.contains("scope=quick"));
        assert!(payload.contains("fully_verified=true"));
    }

    #[test]
    fn compaction_returns_a_truthful_pack_receipt() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("open library");
        library
            .put_bytes(ObjectKind::Wikitext, b"fixture canonical source")
            .expect("store loose object");
        drop(library);

        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let outcome = handler.mutate(Mutation::Compact).expect("compact");
        assert_eq!(outcome.result, "compaction-complete");
        let payload = String::from_utf8(outcome.payload).expect("UTF-8 receipt");
        assert!(payload.contains("packed=true"));
        assert!(payload.contains("objects=1"));
    }

    #[test]
    fn sync_all_with_no_collections_completes_without_networking() {
        let temporary = TempLibrary::new();
        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let outcome = handler
            .mutate(Mutation::SyncAll)
            .expect("sync empty library");
        assert_eq!(outcome.result, "synchronization-complete");
        assert_eq!(outcome.payload, b"collections=0 pages_checked=0 differing_heads=0 missing_pages=0 revision_batches=0 revisions_enumerated=0 revisions_captured=0 revisions_reused=0 resumed_runs=0");
    }

    #[test]
    fn sync_collection_rejects_a_draft_without_contacting_its_source() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("open library");
        let wiki_id = library
            .register_wiki("http://127.0.0.1:9/w/api.php", "en")
            .expect("register loopback fixture source");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Draft")
            .expect("create draft");
        drop(library);

        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let error = handler
            .mutate(Mutation::SyncCollection(collection_id.get()))
            .expect_err("unconfigured draft must fail");
        assert!(error.message().contains("no committed configuration"));
    }

    #[test]
    fn extension_operations_remain_explicitly_unsupported() {
        let temporary = TempLibrary::new();
        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let error = handler
            .mutate(Mutation::Extension {
                name: "schedule-now".to_owned(),
                payload: Vec::new(),
            })
            .expect_err("extension must not fake success");
        assert_eq!(error.code(), crate::ErrorCode::Unsupported);
    }
}
