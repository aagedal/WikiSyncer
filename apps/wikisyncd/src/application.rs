//! Dispatch from the daemon contract into the existing application crates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::runtime::{Builder, Runtime};
use wikisync_core::CollectionId;
use wikisync_integrity::{VerificationScope, verify_library};
use wikisync_mediawiki::{ClientConfig, MediaWikiClient};
use wikisync_store::{Library, NetworkTransferPolicy, ScheduleCadence};
use wikisync_sync::{ReconciliationReport, reconcile_collection_heads_with_cancellation};

use crate::{
    HandlerStatus, MeteredNetworkState, MeteredNetworkStatus, Mutation, MutationOutcome,
    OperationControl, OperationError, RequestHandler, SET_COLLECTION_SCHEDULE_EXTENSION,
    SET_NETWORK_TRANSFER_POLICY_EXTENSION, canonical_library_root, detect_metered_network,
    next_occurrence_after, recover,
};

const BACKGROUND_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Production mutation dispatcher backed by the durable WikiSyncer application APIs.
///
/// Each call opens the library only after the daemon has acquired the cooperative
/// writer lease, and completes synchronously before another request is accepted.
#[derive(Debug)]
pub struct ApplicationHandler {
    library_root: PathBuf,
    last_operation: Option<String>,
    last_network_status: Option<MeteredNetworkStatus>,
    metered_network_probe: fn() -> MeteredNetworkStatus,
    background_retry_not_before: HashMap<CollectionId, Instant>,
}

impl ApplicationHandler {
    /// Creates a dispatcher for an existing library directory.
    pub fn new(library_root: impl AsRef<Path>) -> Result<Self, crate::DaemonError> {
        Ok(Self {
            library_root: canonical_library_root(library_root.as_ref())?,
            last_operation: None,
            last_network_status: None,
            metered_network_probe: detect_metered_network,
            background_retry_not_before: HashMap::new(),
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
        network_policy: NetworkTransferPolicy,
        control: &OperationControl,
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
        let max_concurrent_requests = usize::try_from(network_policy.max_concurrent_requests())
            .map_err(|_| OperationError::failed("network concurrency policy is too large"))?;
        let max_downloaded_response_bytes_per_second = network_policy
            .max_download_bytes_per_second()
            .map(usize::try_from)
            .transpose()
            .map_err(|_| OperationError::failed("network byte-rate policy is too large"))?;
        let client_config = ClientConfig::new(&wiki.api_endpoint, user_agent())
            .and_then(|config| config.with_max_concurrent_requests(max_concurrent_requests))
            .and_then(|config| {
                config.with_max_downloaded_response_bytes_per_second(
                    max_downloaded_response_bytes_per_second,
                )
            })
            .map_err(|error| OperationError::failed(error.to_string()))?;
        let client = MediaWikiClient::new(client_config)
            .map_err(|error| OperationError::failed(error.to_string()))?;
        runtime
            .block_on(reconcile_collection_heads_with_cancellation(
                &client,
                library,
                configuration.wiki_id,
                collection_id,
                checkpoint_candidate,
                &|| control.is_shutdown_requested(),
            ))
            .map_err(|error| OperationError::failed(error.to_string()))
    }

    fn sync_one(
        &mut self,
        raw_collection_id: u64,
        control: &OperationControl,
    ) -> Result<MutationOutcome, OperationError> {
        let collection_id = CollectionId::new(raw_collection_id)
            .map_err(|error| OperationError::failed(error.to_string()))?;
        let mut library = Library::open(&self.library_root).map_err(operation_failed)?;
        let network_policy = library
            .network_transfer_policy()
            .map_err(operation_failed)?;
        self.enforce_metered_policy(network_policy)?;
        let runtime = Self::runtime()?;
        let report = Self::sync_collection(
            &runtime,
            &mut library,
            collection_id,
            unix_time()?,
            network_policy,
            control,
        )?;
        self.background_retry_not_before.remove(&collection_id);
        let payload = reconciliation_payload(1, &report);
        self.last_operation = Some(format!("synchronized collection {collection_id}"));
        Ok(MutationOutcome {
            result: "synchronization-complete".to_owned(),
            payload: payload.into_bytes(),
        })
    }

    fn sync_all(&mut self, control: &OperationControl) -> Result<MutationOutcome, OperationError> {
        let mut library = Library::open(&self.library_root).map_err(operation_failed)?;
        let network_policy = library
            .network_transfer_policy()
            .map_err(operation_failed)?;
        self.enforce_metered_policy(network_policy)?;
        let collections = library.collections().map_err(operation_failed)?;
        let runtime = Self::runtime()?;
        let checkpoint_candidate = unix_time()?;
        let mut totals = ReconciliationTotals::default();
        for collection in &collections {
            if control.is_shutdown_requested() {
                return Err(OperationError::failed(
                    "synchronization cancelled by shutdown request",
                ));
            }
            let report = Self::sync_collection(
                &runtime,
                &mut library,
                collection.collection_id,
                checkpoint_candidate,
                network_policy,
                control,
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
            "scope={} coverage={:?} objects_examined={} objects_verified={} canonical_bytes_verified={} manifests_examined={} manifests_identity_verified={} findings={} omitted_findings={} fully_verified={fully_verified}",
            if full { "full" } else { "quick" },
            report.coverage,
            report.objects_examined,
            report.objects_verified,
            report.canonical_bytes_verified,
            report.manifests_examined,
            report.manifests_identity_verified,
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

    fn set_schedule(&mut self, payload: &[u8]) -> Result<MutationOutcome, OperationError> {
        let update = decode_schedule_update(payload)?;
        let now = unix_time()?;
        let next_run_at = next_occurrence_after(
            update.cadence,
            update.collection_id.get(),
            update.jitter_seconds,
            now,
        );
        let mut library = Library::open(&self.library_root).map_err(operation_failed)?;
        let schedule = library
            .set_collection_schedule(
                update.collection_id,
                update.cadence,
                update.jitter_seconds,
                update.paused,
                next_run_at,
            )
            .map_err(operation_failed)?;
        self.last_operation = Some(format!(
            "configured schedule for collection {}",
            update.collection_id
        ));
        Ok(MutationOutcome {
            result: "schedule-configured".to_owned(),
            payload: format!(
                "collection_id={} cadence={} paused={} jitter_seconds={} next_run_at={}",
                schedule.collection_id,
                cadence_label(schedule.cadence),
                schedule.paused,
                schedule.jitter_seconds,
                schedule
                    .next_run_at
                    .map_or_else(|| "none".to_owned(), |value| value.to_string())
            )
            .into_bytes(),
        })
    }

    fn set_network_policy(&mut self, payload: &[u8]) -> Result<MutationOutcome, OperationError> {
        let policy = decode_network_policy(payload)?;
        let mut library = Library::open(&self.library_root).map_err(operation_failed)?;
        library
            .update_network_transfer_policy(policy)
            .map_err(operation_failed)?;
        self.last_operation = Some("configured network transfer policy".to_owned());
        Ok(MutationOutcome {
            result: "network-transfer-policy-configured".to_owned(),
            payload: format!(
                "max_concurrent_requests={} max_download_bytes_per_second={} avoid_metered_networks={}",
                policy.max_concurrent_requests(),
                policy
                    .max_download_bytes_per_second()
                    .map_or_else(|| "unlimited".to_owned(), |value| value.to_string()),
                policy.avoid_metered_networks(),
            )
            .into_bytes(),
        })
    }

    fn poll_schedule(
        &mut self,
        control: &OperationControl,
    ) -> Result<Option<MutationOutcome>, OperationError> {
        if control.is_shutdown_requested() {
            return Ok(None);
        }
        let now = unix_time()?;
        let mut library = Library::open(&self.library_root).map_err(operation_failed)?;
        let network_policy = library
            .network_transfer_policy()
            .map_err(operation_failed)?;
        if network_policy.avoid_metered_networks()
            && self.metered_status().state == MeteredNetworkState::Metered
        {
            self.last_operation =
                Some("automatic synchronization is waiting for an unmetered network".to_owned());
            return Ok(None);
        }
        let retry_now = Instant::now();
        let unfinished = library
            .running_collection_reconciliations(100)
            .map_err(operation_failed)?
            .into_iter()
            .find(|run| {
                run.collection_id.is_some_and(|collection_id| {
                    self.background_retry_not_before
                        .get(&collection_id)
                        .is_none_or(|not_before| *not_before <= retry_now)
                })
            });
        if let Some(run) = unfinished {
            let collection_id = run
                .collection_id
                .expect("collection reconciliation query excludes source-wide runs");
            drop(library);
            return match self.sync_one(collection_id.get(), control) {
                Ok(outcome) => Ok(Some(outcome)),
                Err(error) => {
                    self.background_retry_not_before
                        .insert(collection_id, Instant::now() + BACKGROUND_RETRY_DELAY);
                    self.last_operation = Some(format!(
                        "resuming synchronization of collection {collection_id} failed: {}",
                        error.message()
                    ));
                    Err(error)
                }
            };
        }
        let Some(schedule) = library
            .due_schedules(now, 1)
            .map_err(operation_failed)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let decision = recover(
            schedule.cadence,
            schedule.collection_id.get(),
            schedule.jitter_seconds,
            schedule.next_run_at,
            now,
        );
        let (Some(due_at), Some(next_run_at)) = (decision.due_at, decision.next_run_at) else {
            self.last_operation = Some(format!(
                "schedule for collection {} cannot advance past the current clock",
                schedule.collection_id
            ));
            return Ok(None);
        };
        let claimed = library
            .claim_due_schedule(schedule.collection_id, due_at, now, next_run_at)
            .map_err(operation_failed)?;
        drop(library);
        if claimed.is_none() {
            return Ok(None);
        }
        match self.sync_one(schedule.collection_id.get(), control) {
            Ok(outcome) => Ok(Some(outcome)),
            Err(error) => {
                self.background_retry_not_before.insert(
                    schedule.collection_id,
                    Instant::now() + BACKGROUND_RETRY_DELAY,
                );
                self.last_operation = Some(format!(
                    "scheduled synchronization of collection {} failed: {}",
                    schedule.collection_id,
                    error.message()
                ));
                Err(error)
            }
        }
    }

    fn metered_status(&mut self) -> MeteredNetworkStatus {
        let status = (self.metered_network_probe)();
        self.last_network_status = Some(status);
        status
    }

    fn enforce_metered_policy(
        &mut self,
        network_policy: NetworkTransferPolicy,
    ) -> Result<(), OperationError> {
        if !network_policy.avoid_metered_networks() {
            return Ok(());
        }
        let status = self.metered_status();
        if status.state == MeteredNetworkState::Metered {
            return Err(OperationError::failed(
                "synchronization is blocked by the library policy while the active network is metered",
            ));
        }
        Ok(())
    }
}

impl RequestHandler for ApplicationHandler {
    fn status(&self) -> HandlerStatus {
        let network_detail = self.last_network_status.map_or_else(String::new, |status| {
            format!(
                "; metered-network probe: {:?} ({:?})",
                status.state, status.outcome
            )
        });
        HandlerStatus {
            state: "idle".to_owned(),
            detail: format!(
                "{}{}",
                self.last_operation
                    .clone()
                    .unwrap_or_else(|| "application mutation dispatcher is ready".to_owned()),
                network_detail
            ),
        }
    }

    fn mutate(
        &mut self,
        mutation: Mutation,
        control: OperationControl,
    ) -> Result<MutationOutcome, OperationError> {
        match mutation {
            Mutation::SyncAll => self.sync_all(&control),
            Mutation::SyncCollection(collection_id) => self.sync_one(collection_id, &control),
            Mutation::Verify { full } => self.verify(full),
            Mutation::Compact => self.compact(),
            Mutation::Extension { name, payload } if name == SET_COLLECTION_SCHEDULE_EXTENSION => {
                self.set_schedule(&payload)
            }
            Mutation::Extension { name, payload }
                if name == SET_NETWORK_TRANSFER_POLICY_EXTENSION =>
            {
                self.set_network_policy(&payload)
            }
            Mutation::Extension { name, .. } => Err(OperationError::unsupported(format!(
                "extension operation {name:?} is not implemented"
            ))),
        }
    }

    fn poll_background(
        &mut self,
        control: OperationControl,
    ) -> Result<Option<MutationOutcome>, OperationError> {
        self.poll_schedule(&control)
    }
}

fn decode_network_policy(payload: &[u8]) -> Result<NetworkTransferPolicy, OperationError> {
    if payload.len() != 13 {
        return Err(OperationError::failed(
            "network policy extension payload must be exactly 13 bytes",
        ));
    }
    let max_concurrent_requests = u32::from_be_bytes(
        payload[0..4]
            .try_into()
            .map_err(|_| OperationError::failed("invalid network concurrency"))?,
    );
    let encoded_rate = u64::from_be_bytes(
        payload[4..12]
            .try_into()
            .map_err(|_| OperationError::failed("invalid network byte rate"))?,
    );
    let avoid_metered_networks = match payload[12] {
        0 => false,
        1 => true,
        _ => {
            return Err(OperationError::failed(
                "invalid metered-network policy flag",
            ));
        }
    };
    NetworkTransferPolicy::new(
        max_concurrent_requests,
        (encoded_rate != 0).then_some(encoded_rate),
        avoid_metered_networks,
    )
    .map_err(operation_failed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScheduleUpdate {
    collection_id: CollectionId,
    cadence: ScheduleCadence,
    jitter_seconds: u32,
    paused: bool,
}

fn decode_schedule_update(payload: &[u8]) -> Result<ScheduleUpdate, OperationError> {
    if payload.len() != 18 {
        return Err(OperationError::failed(
            "schedule extension payload must be exactly 18 bytes",
        ));
    }
    let collection_id = CollectionId::new(u64::from_be_bytes(
        payload[0..8]
            .try_into()
            .map_err(|_| OperationError::failed("invalid schedule collection ID"))?,
    ))
    .map_err(|error| OperationError::failed(error.to_string()))?;
    let value = u32::from_be_bytes(
        payload[9..13]
            .try_into()
            .map_err(|_| OperationError::failed("invalid schedule cadence value"))?,
    );
    let cadence = match payload[8] {
        0 if value == 0 => ScheduleCadence::Manual,
        1 => ScheduleCadence::interval(value).map_err(operation_failed)?,
        2 => ScheduleCadence::daily_utc(value).map_err(operation_failed)?,
        _ => return Err(OperationError::failed("invalid schedule cadence encoding")),
    };
    let jitter_seconds = u32::from_be_bytes(
        payload[13..17]
            .try_into()
            .map_err(|_| OperationError::failed("invalid schedule jitter"))?,
    );
    let paused = match payload[17] {
        0 => false,
        1 => true,
        _ => return Err(OperationError::failed("invalid schedule pause flag")),
    };
    Ok(ScheduleUpdate {
        collection_id,
        cadence,
        jitter_seconds,
        paused,
    })
}

fn cadence_label(cadence: ScheduleCadence) -> &'static str {
    match cadence {
        ScheduleCadence::Manual => "manual",
        ScheduleCadence::Interval(_) => "interval",
        ScheduleCadence::DailyUtc(_) => "daily-utc",
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use wikisync_core::{
        CollectionBudget, CollectionRemovalPolicy, CollectionRule, HistoryPolicy, PageTitle,
        TitleSelection,
    };
    use wikisync_store::ObjectKind;
    use wikisync_sync::capture_explicit_titles;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    const TITLE_RESOLUTION: &str =
        include_str!("../../../fixtures/mediawiki/title-resolution.json");
    const REVISION_CONTENT: &str =
        include_str!("../../../fixtures/mediawiki/revision-content.json");
    const RECONCILIATION_TITLE_RESOLUTION: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-title-resolution.json");
    const RECONCILIATION_REVISIONS: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-revisions.json");
    const RECONCILIATION_CONTENT_MIDDLE: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-content-middle.json");
    const RECONCILIATION_CONTENT_HEAD: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-content-head.json");
    const RECONCILIATION_REVISIONS_FROM_MIDDLE: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-revisions-from-middle.json");
    const MAXLAG: &str = include_str!("../../../fixtures/mediawiki/maxlag.json");

    fn operation_control() -> OperationControl {
        OperationControl::running()
    }

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

    #[derive(Clone, Copy, Debug)]
    struct FixtureResponse {
        body: &'static str,
        retry_after: Option<u64>,
    }

    impl FixtureResponse {
        const fn json(body: &'static str) -> Self {
            Self {
                body,
                retry_after: None,
            }
        }

        const fn throttled(body: &'static str) -> Self {
            Self {
                body,
                retry_after: Some(0),
            }
        }
    }

    #[derive(Debug)]
    struct FixtureServer {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FixtureServer {
        fn start(responses: Vec<FixtureResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
            let address = listener.local_addr().expect("fixture server address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let thread = thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().expect("accept fixture request");
                    let request = read_fixture_request(&mut stream);
                    captured.lock().expect("request lock").push(request);
                    write_fixture_response(&mut stream, response);
                }
            });
            Self {
                endpoint: format!("http://{address}/w/api.php"),
                requests,
                thread: Some(thread),
            }
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }

        fn finish(mut self) -> Vec<String> {
            self.thread
                .take()
                .expect("fixture thread")
                .join()
                .expect("fixture server did not panic");
            Arc::try_unwrap(self.requests)
                .expect("all fixture handles dropped")
                .into_inner()
                .expect("request lock")
        }
    }

    fn read_fixture_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1_024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).expect("read fixture request");
            assert!(read > 0, "client closed before request headers");
            bytes.extend_from_slice(&buffer[..read]);
            assert!(bytes.len() <= 64 * 1_024, "fixture headers too large");
        }
        String::from_utf8(bytes).expect("fixture headers are UTF-8 compatible")
    }

    fn write_fixture_response(stream: &mut TcpStream, response: FixtureResponse) {
        let retry_after = response
            .retry_after
            .map(|seconds| format!("Retry-After: {seconds}\r\n"))
            .unwrap_or_default();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{retry_after}Content-Length: {}\r\nConnection: close\r\n\r\n",
            response.body.len()
        );
        stream
            .write_all(headers.as_bytes())
            .expect("write fixture headers");
        stream
            .write_all(response.body.as_bytes())
            .expect("write fixture body");
    }

    #[test]
    fn verification_reports_completion_without_overclaiming_integrity() {
        let temporary = TempLibrary::new();
        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let outcome = handler
            .mutate(Mutation::Verify { full: false }, operation_control())
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
        let outcome = handler
            .mutate(Mutation::Compact, operation_control())
            .expect("compact");
        assert_eq!(outcome.result, "compaction-complete");
        let payload = String::from_utf8(outcome.payload).expect("UTF-8 receipt");
        assert!(payload.contains("packed=true"));
        assert!(payload.contains("objects=1"));
    }

    #[test]
    fn network_policy_extension_is_validated_and_durable() {
        let temporary = TempLibrary::new();
        let policy = NetworkTransferPolicy::new(7, Some(500_000), true).expect("policy");
        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let outcome = handler
            .mutate(
                crate::set_network_transfer_policy_mutation(policy),
                operation_control(),
            )
            .expect("save network policy");
        assert_eq!(outcome.result, "network-transfer-policy-configured");

        let library = Library::open(temporary.path()).expect("reopen library");
        assert_eq!(library.network_transfer_policy().unwrap(), policy);

        let error = handler
            .mutate(
                Mutation::Extension {
                    name: SET_NETWORK_TRANSFER_POLICY_EXTENSION.to_owned(),
                    payload: vec![0; 12],
                },
                operation_control(),
            )
            .expect_err("truncated policy must fail");
        assert_eq!(error.code(), crate::ErrorCode::OperationFailed);
    }

    #[test]
    fn sync_all_with_no_collections_completes_without_networking() {
        let temporary = TempLibrary::new();
        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let outcome = handler
            .mutate(Mutation::SyncAll, operation_control())
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
            .mutate(
                Mutation::SyncCollection(collection_id.get()),
                operation_control(),
            )
            .expect_err("unconfigured draft must fail");
        assert!(error.message().contains("no committed configuration"));
    }

    #[test]
    fn extension_operations_remain_explicitly_unsupported() {
        let temporary = TempLibrary::new();
        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let error = handler
            .mutate(
                Mutation::Extension {
                    name: "schedule-now".to_owned(),
                    payload: Vec::new(),
                },
                operation_control(),
            )
            .expect_err("extension must not fake success");
        assert_eq!(error.code(), crate::ErrorCode::Unsupported);
    }

    #[test]
    fn schedule_extension_persists_a_recurring_configuration() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("open library");
        let wiki_id = library
            .register_wiki("http://127.0.0.1:9/w/api.php", "en")
            .expect("register source");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Scheduled")
            .expect("create collection");
        drop(library);

        let cadence = ScheduleCadence::interval(3_600).expect("interval");
        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let outcome = handler
            .mutate(
                crate::set_collection_schedule_mutation(collection_id.get(), cadence, 300, false),
                operation_control(),
            )
            .expect("configure schedule");
        assert_eq!(outcome.result, "schedule-configured");

        let library = Library::open(temporary.path()).expect("reopen library");
        let stored = library
            .collection_schedule(collection_id)
            .expect("read schedule")
            .expect("schedule exists");
        assert_eq!(stored.cadence, cadence);
        assert_eq!(stored.jitter_seconds, 300);
        assert!(!stored.paused);
        assert!(stored.next_run_at.is_some());
    }

    #[test]
    fn metered_policy_leaves_an_overdue_schedule_unclaimed() {
        fn metered() -> MeteredNetworkStatus {
            MeteredNetworkStatus {
                state: MeteredNetworkState::Metered,
                outcome: crate::MeteredNetworkProbeOutcome::Reported,
            }
        }

        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("open library");
        let wiki_id = library
            .register_wiki("http://127.0.0.1:9/w/api.php", "en")
            .expect("register source");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Metered")
            .expect("create collection");
        let due_at = unix_time().expect("clock");
        library
            .set_collection_schedule(
                collection_id,
                ScheduleCadence::interval(60).expect("interval"),
                0,
                false,
                Some(due_at),
            )
            .expect("set overdue schedule");
        library
            .update_network_transfer_policy(
                NetworkTransferPolicy::new(4, None, true).expect("policy"),
            )
            .expect("save policy");
        drop(library);

        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        handler.metered_network_probe = metered;
        assert_eq!(
            handler
                .poll_background(operation_control())
                .expect("metered poll"),
            None
        );
        assert!(
            handler
                .status()
                .detail
                .contains("waiting for an unmetered network")
        );
        let error = handler
            .mutate(Mutation::SyncAll, operation_control())
            .expect_err("foreground sync must honor metered policy");
        assert!(error.message().contains("blocked by the library policy"));

        let library = Library::open(temporary.path()).expect("reopen library");
        let stored = library
            .collection_schedule(collection_id)
            .expect("read schedule")
            .expect("schedule exists");
        assert_eq!(stored.next_run_at, Some(due_at));
        assert_eq!(stored.last_started_at, None);
    }

    #[test]
    fn overdue_schedule_is_claimed_once_before_failed_sync() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("open library");
        let wiki_id = library
            .register_wiki("http://127.0.0.1:9/w/api.php", "en")
            .expect("register source");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Draft")
            .expect("create draft");
        let due_at = unix_time().expect("clock");
        library
            .set_collection_schedule(
                collection_id,
                ScheduleCadence::interval(60).expect("interval"),
                0,
                false,
                Some(due_at),
            )
            .expect("set overdue schedule");
        drop(library);

        let mut handler = ApplicationHandler::new(temporary.path()).expect("handler");
        let error = handler
            .poll_background(operation_control())
            .expect_err("draft synchronization must fail after claim");
        assert!(error.message().contains("no committed configuration"));
        assert_eq!(
            handler
                .poll_background(operation_control())
                .expect("second poll"),
            None
        );

        let library = Library::open(temporary.path()).expect("reopen library");
        let stored = library
            .collection_schedule(collection_id)
            .expect("read schedule")
            .expect("schedule exists");
        assert!(
            stored
                .last_started_at
                .is_some_and(|started| started >= due_at)
        );
        assert!(stored.next_run_at.is_some_and(|next| next > due_at));
    }

    #[test]
    fn real_daemon_coalesces_an_overdue_run_and_does_not_repeat_after_restart() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("open library");
        let wiki_id = library
            .register_wiki("http://127.0.0.1:9/w/api.php", "en")
            .expect("register source");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([PageTitle::new("Offline fixture").expect("title")])
                .expect("selection"),
        );
        let collection_id = library
            .create_collection(
                wiki_id,
                "Scheduled",
                &rule,
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited(),
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect("create configured collection");
        let due_at = unix_time().expect("clock");
        library
            .set_collection_schedule(
                collection_id,
                ScheduleCadence::interval(3_600).expect("interval"),
                0,
                false,
                Some(due_at),
            )
            .expect("set overdue schedule");
        drop(library);

        let daemon = crate::Daemon::bind(
            temporary.path(),
            ApplicationHandler::new(temporary.path()).expect("handler"),
        )
        .expect("bind daemon");
        let daemon_thread = thread::spawn(move || daemon.run());
        let client = crate::Client::for_library(temporary.path()).expect("client");
        let mut claimed = None;
        for _ in 0..150 {
            let library = Library::open(temporary.path()).expect("observe library");
            claimed = library
                .collection_schedule(collection_id)
                .expect("read schedule")
                .and_then(|schedule| schedule.last_started_at);
            if claimed.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            claimed.is_some(),
            "daemon did not claim the overdue schedule"
        );
        assert_eq!(client.status().expect("status").completed_mutations, 1);
        client.shutdown().expect("shutdown first daemon");
        daemon_thread.join().expect("join").expect("daemon run");

        let daemon = crate::Daemon::bind(
            temporary.path(),
            ApplicationHandler::new(temporary.path()).expect("restart handler"),
        )
        .expect("restart daemon");
        let daemon_thread = thread::spawn(move || daemon.run());
        thread::sleep(Duration::from_millis(1_200));
        let client = crate::Client::for_library(temporary.path()).expect("restart client");
        assert_eq!(
            client.status().expect("restart status").completed_mutations,
            0
        );
        let library = Library::open(temporary.path()).expect("observe restarted library");
        assert_eq!(
            library
                .collection_schedule(collection_id)
                .expect("read restarted schedule")
                .and_then(|schedule| schedule.last_started_at),
            claimed
        );
        drop(library);
        client.shutdown().expect("shutdown restarted daemon");
        daemon_thread.join().expect("join").expect("daemon run");
    }

    #[test]
    fn real_daemon_resumes_throttled_partial_sync_after_restart() {
        let server = FixtureServer::start(vec![
            FixtureResponse::json(TITLE_RESOLUTION),
            FixtureResponse::json(REVISION_CONTENT),
            FixtureResponse::json(RECONCILIATION_TITLE_RESOLUTION),
            FixtureResponse::json(RECONCILIATION_REVISIONS),
            FixtureResponse::json(RECONCILIATION_CONTENT_MIDDLE),
            FixtureResponse::throttled(MAXLAG),
            FixtureResponse::throttled(MAXLAG),
            FixtureResponse::throttled(MAXLAG),
            FixtureResponse::throttled(MAXLAG),
            FixtureResponse::json(RECONCILIATION_TITLE_RESOLUTION),
            FixtureResponse::json(RECONCILIATION_REVISIONS_FROM_MIDDLE),
            FixtureResponse::json(RECONCILIATION_CONTENT_HEAD),
        ]);
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("open library");
        let wiki_id = library
            .register_wiki(server.endpoint(), "en")
            .expect("register fixture source");
        let selection =
            TitleSelection::new([PageTitle::new("Rust_programming_language").expect("title")])
                .expect("selection");
        let collection_id = library
            .create_collection(
                wiki_id,
                "Scheduled throttling recovery",
                &CollectionRule::ExplicitTitles(selection.clone()),
                HistoryPolicy::CurrentAndFuture,
                CollectionBudget::unlimited(),
                CollectionRemovalPolicy::StopTrackingRetainHistory,
            )
            .expect("create configured collection");
        let setup_runtime = ApplicationHandler::runtime().expect("setup runtime");
        let setup_client = MediaWikiClient::new(
            ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 daemon-gate-test")
                .expect("fixture client configuration"),
        )
        .expect("fixture client");
        let initial = setup_runtime
            .block_on(capture_explicit_titles(
                &setup_client,
                &mut library,
                wiki_id,
                collection_id,
                &selection,
            ))
            .expect("initial fixture capture");
        let page_id = initial.pages[0].page_id;
        let original_head = initial.pages[0].revision_id;
        let due_at = unix_time().expect("clock");
        library
            .set_collection_schedule(
                collection_id,
                ScheduleCadence::interval(3_600).expect("interval"),
                0,
                false,
                Some(due_at),
            )
            .expect("set overdue schedule");
        drop(setup_client);
        drop(setup_runtime);
        drop(library);

        let daemon = crate::Daemon::bind(
            temporary.path(),
            ApplicationHandler::new(temporary.path()).expect("handler"),
        )
        .expect("bind first daemon");
        let daemon_thread = thread::spawn(move || daemon.run());
        let client = crate::Client::for_library(temporary.path()).expect("client");
        let failed_run = wait_for_sync_run(temporary.path(), |run| run.failed_jobs == 1);
        assert_eq!(failed_run.state, wikisync_store::SyncRunState::Running);
        assert!(
            failed_run
                .latest_error
                .as_ref()
                .is_some_and(|error| error.retryable)
        );

        let library = Library::open(temporary.path()).expect("inspect partial library");
        assert!(
            library
                .revision(
                    wiki_id,
                    wikisync_core::RevisionId::new(1_300_000_002).expect("middle revision")
                )
                .expect("middle lookup")
                .is_some()
        );
        assert!(
            library
                .revision(
                    wiki_id,
                    wikisync_core::RevisionId::new(1_300_000_003).expect("head revision")
                )
                .expect("head lookup")
                .is_none()
        );
        assert_eq!(
            library
                .page(wiki_id, page_id)
                .expect("page lookup")
                .expect("page")
                .current_revision_id,
            Some(original_head)
        );
        assert_eq!(
            library.sync_checkpoints().expect("checkpoint")[0].committed_through,
            0
        );
        drop(library);
        client.shutdown().expect("shutdown first daemon");
        daemon_thread.join().expect("join").expect("daemon run");

        let daemon = crate::Daemon::bind(
            temporary.path(),
            ApplicationHandler::new(temporary.path()).expect("restart handler"),
        )
        .expect("restart daemon");
        let daemon_thread = thread::spawn(move || daemon.run());
        let completed = wait_for_sync_run(temporary.path(), |run| {
            run.run_id == failed_run.run_id && run.state == wikisync_store::SyncRunState::Succeeded
        });
        assert_eq!(completed.run_id, failed_run.run_id);
        let client = crate::Client::for_library(temporary.path()).expect("restart client");
        assert_eq!(client.status().expect("status").completed_mutations, 1);

        let library = Library::open(temporary.path()).expect("inspect completed library");
        assert_eq!(
            library
                .page(wiki_id, page_id)
                .expect("page lookup")
                .expect("page")
                .current_revision_id
                .expect("head")
                .get(),
            1_300_000_003
        );
        assert_eq!(
            library
                .revisions_for_page(wiki_id, page_id)
                .expect("history")
                .len(),
            3
        );
        assert!(library.sync_checkpoints().expect("checkpoint")[0].committed_through > 0);
        drop(library);
        client.shutdown().expect("shutdown restarted daemon");
        daemon_thread.join().expect("join").expect("daemon run");

        let requests = server.finish();
        assert_eq!(requests.len(), 12);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("rvstartid=1300000002"))
                .count(),
            2,
            "middle revision should be fetched once and later reused as the resume anchor"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("rvstartid=1300000003"))
                .count(),
            5,
            "the head is attempted four bounded times, then once after restart"
        );
    }

    fn wait_for_sync_run(
        library_root: &Path,
        predicate: impl Fn(&wikisync_store::SyncRunStatus) -> bool,
    ) -> wikisync_store::SyncRunStatus {
        for _ in 0..500 {
            let library = Library::open(library_root).expect("observe library");
            if let Some(run) = library
                .sync_run_statuses(20)
                .expect("run statuses")
                .into_iter()
                .find(&predicate)
            {
                return run;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("daemon did not reach expected synchronization state");
    }
}
